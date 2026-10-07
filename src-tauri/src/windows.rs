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

fn unix_secs() -> u64 {
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
    suspended: bool,
}

/// Live per-window bookkeeping, managed as Tauri state.
#[derive(Default)]
pub struct WindowState {
    inner: Mutex<HashMap<String, TrackedWindow>>,
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

/// Open an account's window, or focus it if it is already open. The window is
/// lazily created here — nothing exists until the user opens the account.
pub fn open_account(
    app: &AppHandle,
    store: &AppStore,
    adblock: &AdblockState,
    winstate: &WindowState,
    app_id: &str,
    account_id: &str,
) -> Result<(), String> {
    open_account_placed(app, store, adblock, winstate, app_id, account_id, None)
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
        if let Some(p) = placement {
            apply_placement(&window, p);
        }
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
    // The stored absolute path is the source of truth for the data directory.
    let session_dir = store.session_dir_for(app_id, account_id)?;

    // Per-account override wins; None means "inherit the app setting".
    let popup_policy = account
        .popup_policy
        .clone()
        .unwrap_or_else(|| web_app.settings.popup_policy.clone());
    let mut builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(page_url.clone()))
        .data_directory(session_dir.clone())
        .title(&title);
    // v0.9.6 (Windows): frameless — the native title bar and its X go away,
    // replaced by our own slim caption strip (caption.rs). tao keeps border
    // resizing working on frameless windows. Linux keeps native decorations.
    #[cfg(windows)]
    {
        builder = builder.decorations(false);
    }
    // v0.9.0: tiled routines place the window at build time (no visible
    // jump); untiled opens keep the classic centered 1200x800.
    // WebviewWindowBuilder::position takes logical pixels directly.
    if let Some(p) = placement {
        builder = builder.position(p.x, p.y).inner_size(p.width, p.height);
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
                    crate::tint::request_retint(&app, &lbl, &u, crate::tint::TintTarget::PageStrip)
                })
                .ok();
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
    // v0.12.0: custom page cursor. Empty string when "off" — skipped.
    let cursor_style = app
        .try_state::<Mutex<crate::launcher_settings::LauncherSettings>>()
        .and_then(|s| s.lock().ok().map(|s| s.custom_cursor.clone()))
        .unwrap_or_default();
    let cursor_js = cursor_chrome_js(&cursor_style);
    if !cursor_js.is_empty() {
        builder = builder.initialization_script(cursor_js);
    }
    let css = adblock.cosmetic_css_for(&web_app.url);
    if !css.is_empty() {
        builder = builder.initialization_script(cosmetic_init_script(&css));
    }
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
                suspended: false,
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
                touch_window(&track_app, &track_label, *focused);
            }
            WindowEvent::Destroyed => {
                // Closed via the X button (not through close_tracked_window):
                // drop the tracked entry so it can't go stale — the watchers
                // tolerate staleness, but the session must not resurrect a
                // window the user closed — and persist the session.
                if let Some(winstate) = track_app.try_state::<WindowState>() {
                    if let Ok(mut tracked) = winstate.inner.lock() {
                        tracked.remove(&track_label);
                    }
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

    store.touch_account(app_id, account_id);
    // A window opened (or re-focused above): persist the session.
    crate::session::write_session(app);
    Ok(())
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
pub(crate) fn cursor_chrome_js(style: &str) -> String {
    let dots: usize = match style {
        "dot" | "ring" => 1,
        "trail" => 6,
        _ => return String::new(),
    };
    let shape_css = match style {
        "ring" => {
            "width:26px;height:26px;margin:-13px 0 0 -13px;\
             border:2px solid #8b5cf6;border-radius:50%;background:transparent;"
        }
        // "dot" and the trail head share the dot look; trail followers
        // are smaller and fade via the per-dot opacity below.
        _ => {
            "width:8px;height:8px;margin:-4px 0 0 -4px;\
             border-radius:50%;background:#8b5cf6;"
        }
    };
    format!(
        r#"(function () {{
  try {{
    var s = document.createElement("style");
    s.setAttribute("data-appmaka", "cursor");
    s.textContent = "html,body,*,*::before,*::after{{cursor:none!important}}";
    (document.head || document.documentElement).appendChild(s);

    var N = {dots};
    var nodes = [];
    for (var i = 0; i < N; i++) {{
      var d = document.createElement("div");
      d.setAttribute("data-appmaka", "cursor-dot");
      d.style.cssText =
        "position:fixed;left:0;top:0;z-index:2147483647;" +
        "pointer-events:none;opacity:0;" +
        {shape_css_json};
      if (N > 1) {{
        // Trail followers shrink and fade with distance from the head.
        var f = 1 - i / N;
        d.style.opacity = "";
        d.style.width = Math.max(3, Math.round(8 * f)) + "px";
        d.style.height = d.style.width;
        var m = -Math.max(3, Math.round(8 * f)) / 2;
        d.style.margin = m + "px 0 0 " + m + "px";
      }}
      document.documentElement.appendChild(d);
      nodes.push({{ el: d, x: -100, y: -100, o: N > 1 ? 0.9 * (1 - i / N) + 0.1 : 1 }});
    }}

    var tx = -100, ty = -100, inside = false;
    document.addEventListener("mousemove", function (e) {{
      tx = e.clientX; ty = e.clientY;
      if (!inside) {{
        inside = true;
        for (var i = 0; i < nodes.length; i++) nodes[i].el.style.opacity = nodes[i].o;
      }}
    }}, {{ passive: true }});
    document.addEventListener("mouseleave", function () {{
      inside = false;
      for (var i = 0; i < nodes.length; i++) nodes[i].el.style.opacity = 0;
    }});

    // One rAF loop; the head snaps, followers lerp for the trail effect.
    // transform-only: no layout, no paint beyond the dots themselves.
    (function frame() {{
      var px = tx, py = ty;
      for (var i = 0; i < nodes.length; i++) {{
        var n = nodes[i];
        if (i === 0) {{ n.x = px; n.y = py; }}
        else {{ n.x += (px - n.x) * 0.4; n.y += (py - n.y) * 0.4; }}
        n.el.style.transform = "translate(" + n.x + "px," + n.y + "px)";
        px = n.x; py = n.y;
      }}
      requestAnimationFrame(frame);
    }})();
  }} catch (e) {{}}
}})();"#,
        dots = dots,
        shape_css_json = serde_json::to_string(shape_css).unwrap_or_default(),
    )
}

// ---------------------------------------------------------------------------
// Popup policy
// ---------------------------------------------------------------------------

static OAUTH_COUNTER: AtomicU64 = AtomicU64::new(0);

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
})();"#;

/// Everything the popup handler needs, captured by value (the handler is
/// `Fn`, so it can't borrow from the stack frame that creates the window).
/// Shared with the preview flow (`preview.rs`), which passes placeholder
/// ids since the app doesn't exist yet.
pub(crate) struct PopupContext {
    pub(crate) app: AppHandle,
    pub(crate) app_id: String,
    pub(crate) account_id: String,
    pub(crate) app_name: String,
    pub(crate) app_url: String,
    pub(crate) session_dir: PathBuf,
    pub(crate) popup_policy: String,
    pub(crate) popup_allowlist: Vec<String>,
}

/// Build the `on_new_window` handler for an account window.
///
/// Runs on a separate thread on Windows, so it must stay non-blocking: the
/// decision is made synchronously and any window creation is bounced to a
/// dedicated spawned thread (never the calling thread, never the main
/// thread — see `spawn_oauth_modal`). The original request is always
/// denied — allowed popups are re-created as contained windows instead.
///
/// Policy "allow": every new-window request (window.open, target=_blank)
/// becomes a plain contained popup sharing the account's session directory,
/// so logins carry over. Policy "block": only allowlisted hosts get the
/// OAuth modal (with its auto-close script); everything else is denied
/// silently, with no intrusive UI.
///
/// Also used by the preview flow with placeholder ids.
///
/// NOTE: the policy is captured when the account window is built. Changing
/// the per-account or app-level popup policy while the window is open has
/// no effect until the window is closed and reopened — the edit dialogs
/// must say so (frontend copy, coordinator-owned).
pub(crate) fn make_popup_handler(
    ctx: PopupContext,
) -> impl Fn(url::Url, tauri::webview::NewWindowFeatures) -> tauri::webview::NewWindowResponse<tauri::Wry>
       + Send
       + 'static {
    // Origin used by the OAuth modal's best-effort auto-close.
    let home_origin = app_origin(&ctx.app_url);
    move |url: url::Url, _features| {
        if ctx.popup_policy == "allow" {
            spawn_popup_window(&ctx.app, &url, &ctx.app_name, &ctx.session_dir);
        } else {
            // "block": only allowlisted hosts get a contained popup.
            let host = url.host_str().unwrap_or("").to_lowercase();
            if ctx
                .popup_allowlist
                .iter()
                .any(|h| h.eq_ignore_ascii_case(&host))
            {
                spawn_oauth_modal(
                    &ctx.app,
                    &url,
                    &ctx.app_id,
                    &ctx.account_id,
                    &ctx.app_name,
                    &home_origin,
                    &ctx.session_dir,
                );
            }
        }
        tauri::webview::NewWindowResponse::Deny
    }
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
pub(crate) fn spawn_popup_window(app: &AppHandle, url: &url::Url, app_name: &str, session_dir: &Path) {
    let title = format!("{app_name} — popup");
    spawn_contained_window(app, "popup", url, &title, session_dir, None, "popup");
}

/// Open an allowlisted popup as a small modal bound to the SAME session
/// directory, so an OAuth login lands in the right account's cookie jar.
/// The modal self-closes (best-effort) when navigation returns to the app's
/// origin; the user can always close it by hand.
fn spawn_oauth_modal(
    app: &AppHandle,
    url: &url::Url,
    app_id: &str,
    account_id: &str,
    app_name: &str,
    home_origin: &str,
    session_dir: &Path,
) {
    let json_home = serde_json::to_string(home_origin).unwrap_or_else(|_| "\"\"".to_string());
    let autoclose = format!(
        "(function(){{var home={json_home};\
        var t=setInterval(function(){{try{{\
        if(window.location.origin===home){{window.close();clearInterval(t);}}\
        }}catch(e){{}}}},1500);}})();"
    );
    let title = format!("{app_name} — sign-in");
    let log_ctx = format!("{app_id}/{account_id}");
    spawn_contained_window(app, "oauth", url, &title, session_dir, Some(autoclose), &log_ctx);
}

/// Shared contained-window builder for popups and OAuth modals.
///
/// SECURITY POSTURE: these windows load external site URLs only, never our
/// frontend bundle, and `withGlobalTauri` is false crate-wide, so no Tauri
/// IPC (`window.__TAURI__`) is ever injected into them — they cannot invoke
/// backend commands. The only script ever injected is the static,
/// JSON-escaped OAuth auto-close snippet (or none). Nested popups inside a
/// contained window are denied outright, preventing popup loops.
fn spawn_contained_window(
    app: &AppHandle,
    label_prefix: &str,
    url: &url::Url,
    title: &str,
    session_dir: &Path,
    initialization_script: Option<String>,
    log_ctx: &str,
) {
    let n = OAUTH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let label = format!("{label_prefix}-{n}");
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
    let window_app = app.clone();
    let url = url.clone();
    let session_dir = session_dir.to_path_buf();
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
            .center()
            // Nested popups inside the contained window are denied outright:
            // an OAuth flow that needs a second popup is rare, and this
            // prevents modal loops.
            .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
            // Popup downloads go through the same in-app manager as every
            // other window (v0.9.8): without this they silently bypassed
            // the download folder, the history, and Mark of the Web.
            .on_download(crate::downloads::make_download_handler(window_app.clone()));
            if let Some(script) = initialization_script {
                builder = builder.initialization_script(script);
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
                        .spawn(move || crate::popup_chrome::tint_popup_caption(&app, &lbl, &u))
                        .ok();
                    true
                });
            }
            match builder.build() {
                Ok(_window) => {
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

fn touch_window(app: &AppHandle, label: &str, focused: bool) {
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
    }
}

/// Restore the pre-suspend title and wake the renderer. WebView2 auto-resumes
/// a suspended page when its controller becomes visible again; the explicit
/// Resume() covers the case where visibility didn't flip.
#[cfg(windows)]
fn resume_window(app: &AppHandle, label: &str) {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_3;
    use windows_core::Interface;

    let Some(window) = app.get_webview_window(label) else {
        return;
    };
    if let Some(winstate) = app.try_state::<WindowState>() {
        if let Ok(tracked) = winstate.inner.lock() {
            if let Some(t) = tracked.get(label) {
                let _ = window.set_title(&t.base_title);
            }
        }
    }
    let _ = window.with_webview(|platform| unsafe {
        if let Ok(core) = platform.controller().CoreWebView2() {
            if let Ok(core3) = core.cast::<ICoreWebView2_3>() {
                let _ = core3.Resume();
            }
        }
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
    // WebView2 refuses TrySuspend while the controller is visible
    // (ERROR_INVALID_STATE), so only suspend hidden windows.
    if !webview_hidden(window) {
        return;
    }
    if try_suspend_webview(window) {
        if let Some(winstate) = app.try_state::<WindowState>() {
            if let Ok(mut tracked) = winstate.inner.lock() {
                if let Some(t) = tracked.get_mut(label) {
                    t.suspended = true;
                    // Visible cue that this window is asleep.
                    let _ = window.set_title(&format!("💤 {}", t.base_title));
                }
            }
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
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_3;
    use windows_core::Interface;
    let done = Arc::new(AtomicBool::new(false));
    let out = done.clone();
    let _ = window.with_webview(move |platform| unsafe {
        if let Ok(core) = platform.controller().CoreWebView2() {
            // ICoreWebView2 -> ICoreWebView2_3 via QueryInterface.
            if let Ok(core3) = core.cast::<ICoreWebView2_3>() {
                // No completion handler: suspension is fire-and-forget, and
                // the title cue is applied by the caller on success.
                if core3.TrySuspend(None).is_ok() {
                    out.store(true, Ordering::Relaxed);
                }
            }
        }
    });
    done.load(Ordering::Relaxed)
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

fn close_tracked_window(app: &AppHandle, label: &str, intent: CloseIntent) {
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
            if !crate::pin::guard_close(app, label) {
                return;
            }
        }
    }
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.close();
    }
    if let Some(winstate) = app.try_state::<WindowState>() {
        if let Ok(mut tracked) = winstate.inner.lock() {
            tracked.remove(label);
        }
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
/// session persistence (v0.9.5): (label, app_id, account_id, opened_at).
/// Cross-checked against live windows so a stale tracked entry (window
/// closed before its Destroyed handler ran) can never resurrect.
pub(crate) fn live_tracked_accounts(app: &AppHandle) -> Vec<(String, String, String, u64)> {
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
        .map(|(label, t)| {
            (
                label.clone(),
                t.app_id.clone(),
                t.account_id.clone(),
                t.opened_at,
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
}

/// Close every open account window. Returns how many were closed. Sessions
/// stay on disk, so reopening an account restores its login. Transient
/// popup children ("popup-*"/"oauth-*") are closed too but not counted —
/// leaving them open would defeat the RAM dashboard's "close all". Sync is
/// fine: closing windows never deadlocks; only *creating* them is
/// restricted.
///
/// Pinned windows (v0.9.9, "Don't close this window") get ONE batch confirm
/// for the whole call, not one per window. "Keep open" (or dismiss) closes
/// the unpinned windows and leaves the pinned ones alone.
#[tauri::command]
#[allow(dead_code)]
pub fn close_all_account_windows(app: AppHandle) -> Result<usize, String> {
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
/// Sync command, same as close_all_account_windows: guard_close may block
/// this thread on the confirm dialog, which is safe on command threads.
#[tauri::command]
pub fn close_open_window(app: AppHandle, label: String) -> Result<(), String> {
    if !close_label_allowed(&label) {
        return Err(format!("not a closeable window: {label}"));
    }
    close_tracked_window(&app, &label, CloseIntent::User);
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
    let tracked: Vec<(String, String, String)> = match app.try_state::<WindowState>() {
        Some(winstate) => match winstate.inner.lock() {
            Ok(map) => map
                .iter()
                .filter(|(l, _)| l.starts_with("acct-"))
                .map(|(l, t)| (l.clone(), t.app_id.clone(), t.account_id.clone()))
                .collect(),
            Err(_) => return Vec::new(),
        },
        None => return Vec::new(),
    };
    tracked
        .into_iter()
        .map(|(label, app_id, account_id)| {
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
    let sys = System::new_with_specifics(
        RefreshKind::nothing()
            .with_processes(ProcessRefreshKind::everything().without_tasks()),
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

    #[test]
    fn cursor_chrome_off_is_empty() {
        assert!(cursor_chrome_js("off").is_empty());
        assert!(cursor_chrome_js("").is_empty());
        assert!(cursor_chrome_js("bogus").is_empty());
    }

    #[test]
    fn cursor_chrome_styles() {
        for style in ["dot", "ring", "trail"] {
            let js = cursor_chrome_js(style);
            assert!(!js.is_empty(), "{style}");
            // Native cursor hidden; dots never intercept clicks.
            assert!(js.contains("cursor:none"), "{style}");
            assert!(js.contains("pointer-events:none"), "{style}");
            // Transform-only movement, one rAF loop.
            assert!(js.contains("requestAnimationFrame"), "{style}");
            assert!(js.contains("translate("), "{style}");
        }
        // Trail has followers; dot/ring are a single node.
        assert!(cursor_chrome_js("trail").contains("var N = 6"));
        assert!(cursor_chrome_js("dot").contains("var N = 1"));
    }
}
