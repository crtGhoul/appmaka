//! "Add to applications" for popup, account, tabbed, and search windows
//! (v0.9.11, extended).
//!
//! Popups are raw site windows with no AppMaka chrome, so their affordances
//! live outside the page: a Windows system-menu item (popup_chrome.rs),
//! the Ctrl+Shift+A global shortcut (registered below), and an "Add"
//! button on the dashboard's popup rows. Account windows get the same
//! shortcut and dashboard button — they capture the window's *live* URL,
//! so a page navigated to inside an account can be promoted to its own app.
//! All paths funnel into `run_add`.
//!
//! A promoted popup opens as a fresh app window (fresh session dir) and the
//! popup closes. A promoted account page is only added to the launcher —
//! the user is mid-browsing, so nothing opens and nothing closes.
//!
//! The search window is addable through the shortcut only: it is frameless
//! on Windows and never gets the popup system menu, and its own shortcuts
//! overlay already advertises Ctrl+Shift+A for this. Promotion never
//! closes the search window — it is reusable, and closing it would destroy
//! the search session and the session its popups share.
//!
//! The new app is created with a FRESH session dir (`store.add_app`): the
//! popup shares its parent account's *live* session dir, which WebView2
//! locks while the parent window lives — moving or copying it is
//! unreliable, so a fresh login is the honest v1. The user was told.
//! The same holds for the search window: its session is wiped on close
//! and shared with its popups, so it is never adopted or copied, and the
//! user signs in once more in the new app window.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri::State;

use std::sync::atomic::{AtomicBool, Ordering};

use crate::adblock::AdblockState;
use crate::store::AppStore;
use crate::windows::WindowState;

/// Fixed global shortcut for the add flow. Fires only when an addable
/// window is focused; ignored silently anywhere else.
const ADD_SHORTCUT: &str = "Ctrl+Shift+A";

/// True only when the fixed shortcut was actually registered by
/// `ensure_add_shortcut_registered` (i.e. nothing else claimed the keys).
/// The global-shortcut handler consults this so the fixed shortcut can
/// never steal a user's own binding or the summon hotkey.
static ADD_SHORTCUT_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn add_shortcut_active() -> bool {
    ADD_SHORTCUT_ACTIVE.load(Ordering::SeqCst)
}

/// Copy deck (spec, verbatim).
pub const MSG_ALREADY_ADDED: &str = "This site is already in your applications.";
pub const MSG_ADD_FAILED: &str = "Couldn't add this site — try the launcher's add box instead.";
pub const MSG_ADDED_PREFIX: &str = "Added to your applications: ";

/// Outcome returned to the dashboard (camelCase for the frontend).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PopupAddOutcome {
    pub already_added: bool,
    pub added: bool,
    pub app_name: String,
}

/// Only `popup-*`, `acct-*`, `tabbed-*` labels and the search window may
/// be promoted. OAuth modals are transient sign-in windows (meaningless
/// to add) and `main` is the launcher. `acct-*` windows capture their
/// *live* URL — the user may have navigated somewhere else and wants
/// that page as a new app. The search window label is an exact match,
/// never a prefix, so lookalikes such as `websearch-1` stay rejected.
pub(crate) fn add_label_allowed(label: &str) -> bool {
    // v0.13.16: tabbed windows join popups and account windows — the
    // page showing in a tab group is addable exactly like a plain page.
    // Search window: the shortcut is its only add path, since it has no
    // system menu and is not an account.
    label.starts_with("popup-")
        || label.starts_with("acct-")
        || label.starts_with("tabbed-")
        || label == crate::websearch::SEARCH_WINDOW_LABEL
}

/// Whether promoting this window opens the new app as a page window.
/// Popups and the search window do; account and tabbed windows stay put
/// and are only added to the launcher. Pure so the matrix stays tested.
pub(crate) fn should_open_new_window(label: &str) -> bool {
    label.starts_with("popup-") || label == crate::websearch::SEARCH_WINDOW_LABEL
}

/// Whether promotion closes the source window. Only popups close: the
/// search window is reusable, and account/tabbed windows stay open.
pub(crate) fn should_close_after_add(label: &str) -> bool {
    label.starts_with("popup-")
}

/// Whether a URL is a search-engine results page rather than a site.
/// DuckDuckGo counts on its bare or www host; Google counts only on a
/// /search path, and its sign-in host never counts. Pure so the guard
/// stays testable without a Tauri app handle. Applied only to the search
/// window, so popup/account behaviour is unchanged.
pub(crate) fn is_search_results_url(raw: &str) -> bool {
    let Ok(parsed) = raw.trim().parse::<url::Url>() else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    if host == "duckduckgo.com" || host == "www.duckduckgo.com" {
        return true;
    }
    if host == "accounts.google.com" {
        return false;
    }
    if host == "google.com" || host.ends_with(".google.com") {
        return parsed.path() == "/search" || parsed.path().starts_with("/search/");
    }
    false
}

/// Validate the popup's live URL: must be a real http(s) address.
/// Pure so it stays unit-testable without a Tauri app handle.
pub(crate) fn validate_popup_url(raw: &str) -> Result<String, String> {
    let url = raw.trim().to_string();
    if !crate::store::is_valid_url(&url) {
        return Err("This page doesn't have an address that can be added.".to_string());
    }
    Ok(url)
}

/// Backend → frontend transient notice (the main window's banner shows it
/// and auto-clears). Used by the fire-and-forget paths (system menu,
/// global shortcut) that have no invoking frontend to answer.
pub(crate) fn emit_notice(app: &AppHandle, message: &str) {
    let _ = app.emit(
        "appmaka:notice",
        serde_json::json!({ "message": message }),
    );
}

/// The full add flow. Blocking HTTP (title/favicon fetch) runs inline: the
/// only callers are the async Tauri command (tokio worker, never the
/// WebView2 IPC thread) and plain spawned threads (system menu,
/// shortcut) — the same safety class as the existing `open_account`
/// command, which also builds windows off the IPC thread.
pub(crate) fn run_add(
    app: &AppHandle,
    store: &AppStore,
    adblock: &AdblockState,
    winstate: &WindowState,
    label: &str,
) -> Result<PopupAddOutcome, String> {
    if !add_label_allowed(label) {
        return Err("That window can't be added to your applications.".to_string());
    }
    let window = app
        .get_webview_window(label)
        .ok_or_else(|| "That window is no longer open.".to_string())?;
    // The backend only knows the popup's *spawn* URL; navigations and
    // logins since then are invisible to it, so read the live address.
    let live_url = window
        .url()
        .map_err(|_| "Couldn't read this page's address.".to_string())?
        .to_string();
    let url = validate_popup_url(&live_url)?;

    // Search window only: a results page is not a site, so refuse it
    // plainly instead of creating a search-engine app.
    if label == crate::websearch::SEARCH_WINDOW_LABEL && is_search_results_url(&url) {
        return Err("This is a search results page — open a result first, then add that site.".to_string());
    }

    let name = crate::page_title::fetch_page_title(&url)
        .unwrap_or_else(|_| crate::preview::prettified_domain(&url));

    // `add_app` dedups by normalized URL internally: an existing app comes
    // back with `created: false` and no duplicate is ever written.
    let outcome = store.add_app(name, url.clone())?;
    let app_id = outcome.app.id.clone();

    if !outcome.created {
        // Already an application: focus its window instead of duplicating.
        // The caller reports `already_added` to the user (dashboard info
        // line, or the notice banner for the fire-and-forget paths).
        let account_id = outcome
            .app
            .accounts
            .first()
            .map(|a| a.id.clone())
            .ok_or_else(|| "That app has no accounts to open.".to_string())?;
        crate::windows::open_account(app, store, adblock, winstate, &app_id, &account_id, false)?;
        return Ok(PopupAddOutcome {
            already_added: true,
            added: false,
            app_name: outcome.app.name,
        });
    }

    // Fresh session dir (created by add_app) — never the popup's live one.
    let account_id = outcome
        .added_account
        .as_ref()
        .map(|a| a.id.clone())
        .ok_or_else(|| "The new app has no account to open.".to_string())?;

    // Best-effort icon: failures keep the tile fallbacks, never the flow.
    if let Ok(page_url) = url.parse::<url::Url>() {
        if let Ok(favicons_dir) = app
            .path()
            .app_data_dir()
            .map(|d| d.join("favicons"))
        {
            if let Some(p) = crate::favicon::download_icon(&page_url, &favicons_dir) {
                let _ =
                    store.set_app_icon(&app_id, Some(p.to_string_lossy().into_owned()));
            }
        }
    }

    // The launcher's grid is a mount-time snapshot: tell it to re-list
    // so the promoted app appears immediately, not after a restart.
    let _ = app.emit("appmaka:apps-changed", ());

    // Open as a proper page window (frameless + caption strip) for
    // popups and the search window: the new window is the feedback that
    // the add worked. A popup then closes, since it has been promoted
    // and keeping both open would show two windows on the same site.
    // The search window never closes — it is reusable, and closing it
    // would destroy the search session its popups share. From an
    // account window the user is mid-browsing — just add it to the
    // launcher and leave everything alone; no new window, no close.
    // The new app's session is fresh (see the module docs), so the user
    // signs in once more there; the search sign-in is not carried over.
    if should_open_new_window(label) {
        crate::windows::open_account(app, store, adblock, winstate, &app_id, &account_id, false)?;
    }
    if should_close_after_add(label) {
        let _ = window.close();
    }

    Ok(PopupAddOutcome {
        already_added: false,
        added: true,
        app_name: outcome.app.name,
    })
}

/// Tauri command (dashboard "Add" button). Async because the flow creates
/// a window — building one inside a synchronous command deadlocks on
/// Windows (wry#583); the async body runs on a tokio worker instead.
#[tauri::command]
pub async fn popup_add_to_applications(
    app: AppHandle,
    store: State<'_, AppStore>,
    adblock: State<'_, AdblockState>,
    winstate: State<'_, WindowState>,
    label: String,
) -> Result<PopupAddOutcome, String> {
    run_add(&app, &store, &adblock, &winstate, &label)
}

/// Numeric id of the fixed shortcut, for the global-shortcut dispatch in
/// main.rs. None when the string doesn't parse (never, but no unwrap).
pub fn add_shortcut_id() -> Option<u32> {
    use std::str::FromStr;
    use tauri_plugin_global_shortcut::Shortcut;
    Shortcut::from_str(ADD_SHORTCUT).ok().map(|s| s.id())
}

/// Register Ctrl+Shift+A unless something already claimed it — a user's
/// own binding or the summon hotkey always wins over the fixed one.
pub fn ensure_add_shortcut_registered(app: &AppHandle) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    if app.global_shortcut().is_registered(ADD_SHORTCUT) {
        return;
    }
    match app.global_shortcut().register(ADD_SHORTCUT) {
        Ok(()) => ADD_SHORTCUT_ACTIVE.store(true, Ordering::SeqCst),
        Err(e) => eprintln!("[appmaka] popup add shortcut not registered: {e}"),
    }
}

/// Global-shortcut dispatch: run the flow only when an addable window
/// is focused; anywhere else the keypress is ignored silently.
pub fn fire_from_hotkey(app: &AppHandle) {
    // Fires on the focused window when it's one we can promote. The
    // label gate in run_add is the authority; this just finds the
    // focused candidate, using the same gate so the two cannot drift.
    let label = app.webview_windows().values().find_map(|w| {
        let l = w.label().to_string();
        (add_label_allowed(&l) && w.is_focused().unwrap_or(false)).then_some(l)
    });
    let Some(label) = label else { return };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let store = app.state::<AppStore>();
        let adblock = app.state::<AdblockState>();
        let winstate = app.state::<WindowState>();
        match run_add(&app, &store, &adblock, &winstate, &label) {
            Ok(o) => {
                if o.already_added {
                    emit_notice(&app, MSG_ALREADY_ADDED);
                } else if !should_open_new_window(&label) {
                    // Windows that open the new app show it as feedback;
                    // account and tabbed windows stay put, so say what
                    // happened.
                    emit_notice(&app, &format!("{MSG_ADDED_PREFIX}{}", o.app_name));
                }
            }
            Err(e) => {
                // Say why it failed when run_add said why (a rejected
                // search results page, for example); the generic message
                // is only the fallback for an empty error.
                if e.is_empty() {
                    emit_notice(&app, MSG_ADD_FAILED);
                } else {
                    emit_notice(&app, &e);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_label_allows_popups_and_accounts() {
        assert!(add_label_allowed("popup-3"));
        assert!(add_label_allowed("popup-0"));
        assert!(add_label_allowed("acct-app1-acct1"));
        assert!(add_label_allowed("tabbed-group1-g0"));
        assert!(!add_label_allowed("oauth-3"));
        assert!(!add_label_allowed("main"));
        assert!(!add_label_allowed(""));
        assert!(!add_label_allowed("popup"));
        assert!(!add_label_allowed("xpopup-3"));
    }

    #[test]
    fn add_label_allows_search_window_exactly() {
        assert!(add_label_allowed(crate::websearch::SEARCH_WINDOW_LABEL));
        assert!(add_label_allowed("websearch"));
        // Exact match only: prefix lookalikes stay rejected.
        assert!(!add_label_allowed("websearch-1"));
        assert!(!add_label_allowed("websearchx"));
        assert!(!add_label_allowed("xwebsearch"));
        assert!(!add_label_allowed("main"));
        assert!(!add_label_allowed("oauth-3"));
        assert!(!add_label_allowed(""));
    }

    #[test]
    fn open_close_matrix() {
        // Popup: open the new window, then close the source.
        assert!(should_open_new_window("popup-3"));
        assert!(should_close_after_add("popup-3"));
        // Search window: open the new window, never close the source.
        assert!(should_open_new_window("websearch"));
        assert!(!should_close_after_add("websearch"));
        // Account and tabbed windows: neither opens nor closes.
        assert!(!should_open_new_window("acct-app1-acct1"));
        assert!(!should_close_after_add("acct-app1-acct1"));
        assert!(!should_open_new_window("tabbed-group1-g0"));
        assert!(!should_close_after_add("tabbed-group1-g0"));
        // Launcher and OAuth: neither.
        assert!(!should_open_new_window("main"));
        assert!(!should_close_after_add("main"));
        assert!(!should_open_new_window("oauth-3"));
        assert!(!should_close_after_add("oauth-3"));
    }

    #[test]
    fn search_results_detection() {
        // DuckDuckGo, with or without www.
        assert!(is_search_results_url("https://duckduckgo.com/?q=example"));
        assert!(is_search_results_url("https://www.duckduckgo.com/?q=example"));
        // Google, only on a /search path.
        assert!(is_search_results_url("https://www.google.com/search?q=example"));
        assert!(is_search_results_url("https://google.com/search?q=example"));
        assert!(!is_search_results_url("https://www.google.com/"));
        // Lookalike paths are not results pages.
        assert!(!is_search_results_url("https://www.google.com/searching"));
        assert!(!is_search_results_url("https://www.google.com/searchfoo"));
        // A normal site is never a results page, even with /search in it.
        assert!(!is_search_results_url("https://example.com/"));
        assert!(!is_search_results_url("https://example.com/search?q=example"));
        // Google's sign-in host never counts.
        assert!(!is_search_results_url("https://accounts.google.com/signin"));
        assert!(!is_search_results_url("https://accounts.google.com/"));
        // Unparseable input is not a results page.
        assert!(!is_search_results_url("not a url"));
        assert!(!is_search_results_url(""));
    }

    #[test]
    fn popup_url_validation_rejects_non_http() {
        assert!(validate_popup_url("https://muse.ai/").is_ok());
        assert!(validate_popup_url("http://localhost:3000/x").is_ok());
        assert!(validate_popup_url("  https://example.com/a?b=c  ").is_ok());
        assert!(validate_popup_url("about:blank").is_err());
        assert!(validate_popup_url("chrome://settings").is_err());
        assert!(validate_popup_url("file:///C:/x.html").is_err());
        assert!(validate_popup_url("javascript:alert(1)").is_err());
        assert!(validate_popup_url("").is_err());
        assert!(validate_popup_url("not a url").is_err());
    }

    #[test]
    fn notice_copy_is_plain_language() {
        assert!(!MSG_ALREADY_ADDED.to_lowercase().contains("popup"));
        assert!(!MSG_ADD_FAILED.to_lowercase().contains("popup"));
        assert!(!MSG_ADD_FAILED.contains("error"));
        assert!(!MSG_ADD_FAILED.contains("Error"));
    }
}
