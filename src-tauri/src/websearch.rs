//! Web-search window (v0.9.3): the launcher's `?query` command opens the
//! search-engine results in a contained in-app webview ("web app") instead
//! of the external browser.
//!
//! This is deliberately NOT an account: no store record, no session
//! persistence, no appearance in the library. Exactly one window exists at
//! a time (fixed label, reused across searches — the RAM discipline), its
//! WebView2/WebKit data dir is wiped when the window closes, and — like
//! every other site window — it loads external URLs only with
//! `withGlobalTauri` false, so no Tauri IPC is ever injected.

use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::atomic::AtomicBool;
#[cfg(windows)]
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};

use crate::adblock::AdblockState;
use crate::launcher_settings::LauncherSettings;
use crate::windows::{apply_placement, cosmetic_init_script, NAV_KEYS_JS, TARGET_BLANK_SHIM_JS, WindowPlacement};

/// Fixed label: one search window at a time; a second search reuses it.
pub const SEARCH_WINDOW_LABEL: &str = "websearch";

/// Build the results URL for a query. Pure (unit-tested): the engine comes
/// from the launcher settings, defaulting to DuckDuckGo — the same two
/// engines and URL shapes the old external-browser path used.
fn build_search_url(engine: &str, query: &str) -> Result<url::Url, String> {
    let base = if engine == "google" {
        "https://www.google.com/search"
    } else {
        "https://duckduckgo.com/"
    };
    url::Url::parse_with_params(base, &[("q", query)])
        .map_err(|e| format!("Couldn't build the search URL: {e}"))
}

fn search_engine(app: &AppHandle) -> String {
    app.try_state::<Mutex<LauncherSettings>>()
        .and_then(|s| s.lock().ok().map(|s| s.search_engine.clone()))
        .unwrap_or_else(|| "duckduckgo".to_string())
}

fn search_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|d| d.join("websearch"))
        .map_err(|e| format!("couldn't resolve the app data dir: {e}"))
}

/// Open the search window for `query`, or navigate the existing one.
/// Sync helper: window *creation* always happens on a dedicated spawned
/// thread (wry#583 — never build on an IPC thread or the main thread);
/// focusing/navigating an existing window is a quick op and safe inline.
///
/// `placement` (v0.9.5) restores saved geometry: a reused window is moved
/// there, a new one is built there so there is no visible jump.
pub fn open_search_window(
    app: &AppHandle,
    adblock: &AdblockState,
    query: &str,
) -> Result<(), String> {
    open_search_window_placed(app, adblock, query, None)
}

pub fn open_search_window_placed(
    app: &AppHandle,
    adblock: &AdblockState,
    query: &str,
    placement: Option<WindowPlacement>,
) -> Result<(), String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("Type something to search for.".to_string());
    }
    let page_url = build_search_url(&search_engine(app), query)?;

    if let Some(window) = app.get_webview_window(SEARCH_WINDOW_LABEL) {
        // Reuse: unminimize + show first (Windows can't focus a minimized
        // window with set_focus alone), retitle, then navigate.
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        if let Some(p) = placement {
            apply_placement(&window, p);
        }
        let js = format!(
            "window.location.href={};",
            serde_json::to_string(page_url.as_str()).unwrap_or_default()
        );
        window
            .eval(&js)
            .map_err(|e| format!("Couldn't open the search: {e}"))?;
        let _ = window.set_title(query);
        // Re-navigation keeps the original open order; only the query text
        // changes in the session.
        crate::session::note_search_opened(app, query);
        crate::session::write_session(app);
        return Ok(());
    }

    let window_app = app.clone();
    let adblock = adblock.clone();
    let title = query.to_string();
    let data_dir = search_data_dir(app)?;
    let _ = std::thread::Builder::new()
        .name("appmaka-websearch".to_string())
        .spawn(move || {
            build_search_window(&window_app, &adblock, &page_url, &title, &data_dir, placement)
        });
    Ok(())
}

/// AppMaka search chrome (v0.12.0): the websearch window loads the engine's
/// own results page, whose stock header looks dated next to the launcher.
/// This replaces it with one quiet AppMaka search field carrying an
/// example placeholder, like the launcher's. Back navigation lives in the
/// native caption strip (v0.12.0 back button); this bar is search only.
///
/// Runs on every document in the search webview, but only activates on
/// engine results hosts; anywhere else it is a no-op. Pure page JS, no
/// IPC — `withGlobalTauri` stays false.
fn search_chrome_js(engine: &str) -> String {
    let (search_base, host_pat) = if engine == "google" {
        ("https://www.google.com/search?q=", "google.com")
    } else {
        ("https://duckduckgo.com/?q=", "duckduckgo.com")
    };
    // Static fallback selectors for the engine header, used when the
    // adaptive walk below finds nothing (engines redesign; the walk is
    // the primary path).
    let fallback_selectors = if engine == "google" {
        r##"["#searchform","header[role='banner']"]"##
    } else {
        r##"["#header","header.header",".header-wrap"]"##
    };
    format!(
        r#"(function () {{
  var SEARCH_BASE = {search_base_json};
  var HOST_PAT = {host_pat_json};
  var FALLBACKS = {fallback_selectors};
  var BAR_ID = "appmaka-searchbar";

  function onResultsHost() {{
    try {{
      return window.location.hostname.indexOf(HOST_PAT) !== -1;
    }} catch (e) {{
      return false;
    }}
  }}

  // Hide the engine's own header: walk up from its search field to the
  // nearest header-like ancestor (adaptive — survives redesigns), with
  // static selectors as fallback.
  function hideEngineHeader() {{
    var q = document.querySelector('input[name="q"]');
    var el = q;
    var depth = 0;
    while (el && el !== document.body && depth < 12) {{
      var tag = el.tagName || "";
      var cls = (typeof el.className === "string" ? el.className : "") || "";
      var id = el.id || "";
      if (tag === "HEADER" || id === "header" || /(^|\s)header(\s|$)/i.test(cls) || /header/i.test(id)) {{
        el.style.setProperty("display", "none", "important");
        return;
      }}
      el = el.parentElement;
      depth++;
    }}
    for (var i = 0; i < FALLBACKS.length; i++) {{
      var n = document.querySelector(FALLBACKS[i]);
      if (n) n.style.setProperty("display", "none", "important");
    }}
  }}

  function currentQuery() {{
    try {{
      return new URLSearchParams(window.location.search).get("q") || "";
    }} catch (e) {{
      return "";
    }}
  }}

  function ensureBar() {{
    if (document.getElementById(BAR_ID)) return;
    var body = document.body;
    if (!body) return;

    var bar = document.createElement("div");
    bar.id = BAR_ID;
    bar.setAttribute("role", "search");

    var input = document.createElement("input");
    input.id = "appmaka-q";
    input.type = "search";
    input.placeholder = "Search the web… e.g. weather tomorrow";
    input.setAttribute("aria-label", "Search the web");
    input.autocomplete = "off";
    input.spellcheck = false;
    try {{ input.value = currentQuery(); }} catch (e) {{}}
    input.addEventListener("keydown", function (e) {{
      if (e.key === "Enter") {{
        var v = input.value.trim();
        if (v) window.location.href = SEARCH_BASE + encodeURIComponent(v);
      }}
    }});

    bar.appendChild(input);
    body.insertBefore(bar, body.firstChild);
    body.style.setProperty("padding-top", "49px", "important");
  }}

  function apply() {{
    if (!onResultsHost()) return;
    try {{ hideEngineHeader(); }} catch (e) {{}}
    try {{ ensureBar(); }} catch (e) {{}}
  }}

  // The engine renders client-side; re-apply as the DOM settles.
  apply();
  try {{
    new MutationObserver(function () {{ apply(); }}).observe(
      document.documentElement,
      {{ childList: true, subtree: true }}
    );
  }} catch (e) {{}}
}})();"#,
        search_base_json = serde_json::to_string(search_base).unwrap_or_default(),
        host_pat_json = serde_json::to_string(host_pat).unwrap_or_default(),
        fallback_selectors = fallback_selectors,
    )
}

/// Companion CSS for the injected search bar. Quiet dark, one violet
/// accent on focus — no decorative gradients (AGENTS.md anti-slop).
const SEARCH_CHROME_CSS: &str = r#"
#appmaka-searchbar {
  position: fixed; top: 0; left: 0; right: 0; z-index: 2147483647;
  display: flex; align-items: center;
  padding: 8px 12px;
  background: #1b1b1d;
  border-bottom: 1px solid rgba(255, 255, 255, 0.08);
  font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
}
#appmaka-q {
  flex: 1 1 auto; height: 32px; border-radius: 8px;
  border: 1px solid rgba(255, 255, 255, 0.14);
  background: rgba(255, 255, 255, 0.06); color: #fff;
  padding: 0 12px; font-size: 14px; outline: none;
}
#appmaka-q:focus { border-color: #8b5cf6; }
#appmaka-q::placeholder { color: rgba(255, 255, 255, 0.38); }
"#;
/// JS: `invoke("open_web_search", { query })`.
/// Async on purpose: window-creating commands are never synchronous
/// (Windows WebView2 deadlock, wry#583) — and creation itself still goes
/// through a dedicated thread via `open_search_window`.
#[tauri::command]
pub async fn open_web_search(app: AppHandle, query: String) -> Result<(), String> {
    let adblock = app
        .try_state::<AdblockState>()
        .as_deref()
        .cloned()
        .ok_or_else(|| "ad-blocker state not initialized".to_string())?;
    open_search_window(&app, &adblock, &query)
}

/// Build the contained search window on a dedicated thread. Mirrors
/// `windows::spawn_contained_window`: external URL only, nested popups
/// denied outright, in-app downloads, Alt+Left/Right nav, cosmetic ad
/// hiding + (Windows) network blocking seeded from the app default (on).
fn build_search_window(
    app: &AppHandle,
    adblock: &AdblockState,
    url: &url::Url,
    title: &str,
    data_dir: &Path,
    placement: Option<WindowPlacement>,
) {
    let mut builder = WebviewWindowBuilder::new(app, SEARCH_WINDOW_LABEL, WebviewUrl::External(url.clone()))
        .data_directory(data_dir.to_path_buf())
        .title(title);
    // v0.9.6 (Windows): frameless + our caption strip, like account
    // windows. Linux keeps native decorations.
    #[cfg(windows)]
    {
        builder = builder.decorations(false);
    }
    // v0.9.5: session restore builds the window at its saved geometry so
    // there is no visible jump; normal opens keep the classic centered
    // 1200x800.
    if let Some(p) = placement {
        builder = builder.position(p.x, p.y).inner_size(p.width, p.height);
    } else {
        builder = builder.inner_size(1200.0, 800.0).center();
    }
    let mut builder = builder
        // A search page's popups stay dead: same posture as OAuth modals.
        .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
        // Downloads from a search page stay in-app (v0.7.0 manager) instead
        // of kicking out to the system browser.
        .on_download(crate::downloads::make_download_handler(app.clone()));
    // target=_blank shim (WebKitGTK drops the clicks otherwise) + Alt+Left/
    // Alt+Right history nav: bare webviews have no chrome.
    builder = builder.initialization_script(TARGET_BLANK_SHIM_JS);
    builder = builder.initialization_script(NAV_KEYS_JS);
    // v0.12.0: AppMaka search chrome — return button + search field with
    // an example placeholder, replacing the engine's dated header. The
    // engine is baked in: the bar navigates with the same URL shape as
    // build_search_url.
    builder = builder.initialization_script(search_chrome_js(&search_engine(app)));
    builder = builder.initialization_script(cosmetic_init_script(SEARCH_CHROME_CSS));
    // v0.12.0: custom page cursor, same as account windows. Skipped when off.
    let cursor_style = app
        .try_state::<Mutex<crate::launcher_settings::LauncherSettings>>()
        .and_then(|s| s.lock().ok().map(|s| s.custom_cursor.clone()))
        .unwrap_or_default();
    let cursor_js = crate::windows::cursor_chrome_js(&cursor_style);
    if !cursor_js.is_empty() {
        builder = builder.initialization_script(cursor_js);
    }
    let css = adblock.cosmetic_css_for(url.as_str());
    if !css.is_empty() {
        builder = builder.initialization_script(cosmetic_init_script(&css));
    }
    // Not an account, so no per-account override: the app default (on).
    // The flag only exists where network blocking does (Windows).
    #[cfg(windows)]
    let adblock_flag = Arc::new(AtomicBool::new(true));
    match builder.build() {
        Ok(window) => {
            #[cfg(windows)]
            crate::adblock::attach_network_blocking(&window, adblock, adblock_flag);
            // v0.9.6: DWM shadow on the frameless window + caption strip
            // (no-ops off Windows).
            #[cfg(windows)]
            let _ = window.set_shadow(true);
            crate::caption::page_window_opened(app, SEARCH_WINDOW_LABEL, &window);
            // The window truly exists now: record it for the session and
            // persist (v0.9.5).
            crate::session::note_search_opened(app, title);
            crate::session::write_session(app);
            // No saved profile: wipe the search data dir when the window
            // closes. Delayed + guarded — a fast reopen recreates the dir,
            // and the guard skips the wipe while a search window is alive.
            let wipe_app = app.clone();
            let wipe_dir = data_dir.to_path_buf();
            window.on_window_event(move |event| {
                match event {
                    WindowEvent::Destroyed => {
                        // Session first: the entry must go even though the
                        // data-dir wipe below is delayed.
                        crate::session::note_search_closed(&wipe_app);
                        crate::session::write_session(&wipe_app);
                        // v0.9.6: drop the caption strip with the window.
                        crate::caption::page_window_closed(SEARCH_WINDOW_LABEL);
                        // Cloned per event: the handler is Fn, called for every
                        // window event, so nothing may move out of it.
                        let wipe_app = wipe_app.clone();
                        let wipe_dir = wipe_dir.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_secs(5));
                            if wipe_app.get_webview_window(SEARCH_WINDOW_LABEL).is_none() {
                                let _ = std::fs::remove_dir_all(&wipe_dir);
                            }
                        });
                    }
                    // Geometry changes feed the session, debounced.
                    WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
                        crate::session::schedule_session_write(&wipe_app);
                        // v0.9.6: keep the caption strip above the window.
                        crate::caption::page_window_moved(SEARCH_WINDOW_LABEL);
                    }
                    _ => {}
                }
            });
        }
        Err(e) => {
            // The session entry was never written (write happens on Ok),
            // but make sure no stale open-record lingers.
            crate::session::note_search_closed(app);
            eprintln!("[appmaka] websearch window failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_url_shapes() {
        let ddg = build_search_url("duckduckgo", "hello world").unwrap();
        assert_eq!(ddg.host_str(), Some("duckduckgo.com"));
        assert_eq!(
            ddg.query_pairs().find(|(k, _)| k == "q").map(|(_, v)| v.into_owned()),
            Some("hello world".to_string())
        );
        let g = build_search_url("google", "a/b?c=d").unwrap();
        assert_eq!(g.host_str(), Some("www.google.com"));
        assert_eq!(
            g.query_pairs().find(|(k, _)| k == "q").map(|(_, v)| v.into_owned()),
            Some("a/b?c=d".to_string())
        );
    }

    #[test]
    fn unknown_engine_falls_back_to_duckduckgo() {
        let u = build_search_url("bing", "x").unwrap();
        assert_eq!(u.host_str(), Some("duckduckgo.com"));
    }

    #[test]
    fn search_window_label_is_fixed_for_reuse() {
        // The whole one-window discipline hangs on this label never varying.
        assert_eq!(SEARCH_WINDOW_LABEL, "websearch");
    }

    #[test]
    fn search_chrome_bakes_engine_url() {
        let ddg = search_chrome_js("duckduckgo");
        assert!(ddg.contains("https://duckduckgo.com/?q="));
        assert!(ddg.contains("duckduckgo.com"));
        let g = search_chrome_js("google");
        assert!(g.contains("https://www.google.com/search?q="));
        assert!(g.contains("google.com"));
        // Unknown engine falls back to DuckDuckGo, matching build_search_url.
        let bing = search_chrome_js("bing");
        assert!(bing.contains("https://duckduckgo.com/?q="));
    }

    #[test]
    fn search_chrome_has_bar_and_example() {
        let js = search_chrome_js("duckduckgo");
        // Search field with an example placeholder, like the launcher's.
        // (Back navigation lives in the native caption strip.)
        assert!(js.contains("appmaka-q"));
        assert!(js.contains("e.g."));
        // Scoped to engine hosts; inert elsewhere.
        assert!(js.contains("onResultsHost"));
    }

    #[test]
    fn search_chrome_css_is_quiet() {
        // Anti-slop: no decorative gradients in the injected bar.
        assert!(!SEARCH_CHROME_CSS.contains("gradient"));
        assert!(SEARCH_CHROME_CSS.contains("#appmaka-searchbar"));
        assert!(SEARCH_CHROME_CSS.contains("#appmaka-q"));
    }
}
