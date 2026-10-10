//! "Add to applications" for popup windows (v0.9.11).
//!
//! A popup is a raw site window with no AppMaka chrome, so the affordances
//! live outside the page: a Windows system-menu item (popup_chrome.rs),
//! the Ctrl+Shift+A global shortcut (registered below), and an "Add"
//! button on the dashboard's popup rows. All three funnel into `run_add`.
//!
//! The new app is created with a FRESH session dir (`store.add_app`): the
//! popup shares its parent account's *live* session dir, which WebView2
//! locks while the parent window lives — moving or copying it is
//! unreliable, so a fresh login is the honest v1. The user was told.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri::State;

use std::sync::atomic::{AtomicBool, Ordering};

use crate::adblock::AdblockState;
use crate::store::AppStore;
use crate::windows::WindowState;

/// Fixed global shortcut for the add flow. Fires only when a popup window
/// is focused; ignored silently anywhere else.
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

/// Outcome returned to the dashboard (camelCase for the frontend).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PopupAddOutcome {
    pub already_added: bool,
    pub added: bool,
    pub app_name: String,
}

/// Only `popup-*` labels may be promoted. OAuth modals are transient
/// sign-in windows (meaningless to add), `main` is the launcher, and
/// `acct-*` windows are already applications.
pub(crate) fn add_label_allowed(label: &str) -> bool {
    label.starts_with("popup-")
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

    // Open as a proper page window (frameless + caption strip), then close
    // the popup: it has been promoted, and keeping both open would show
    // two windows on the same site.
    crate::windows::open_account(app, store, adblock, winstate, &app_id, &account_id, false)?;
    let _ = window.close();

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

/// Global-shortcut dispatch: run the flow only when a popup window is
/// focused; anywhere else the keypress is ignored silently.
pub fn fire_from_hotkey(app: &AppHandle) {
    let label = app.webview_windows().values().find_map(|w| {
        let l = w.label().to_string();
        (l.starts_with("popup-") && w.is_focused().unwrap_or(false)).then_some(l)
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
                }
            }
            Err(_) => emit_notice(&app, MSG_ADD_FAILED),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_label_allows_only_popups() {
        assert!(add_label_allowed("popup-3"));
        assert!(add_label_allowed("popup-0"));
        assert!(!add_label_allowed("oauth-3"));
        assert!(!add_label_allowed("main"));
        assert!(!add_label_allowed("acct-app1-acct1"));
        assert!(!add_label_allowed(""));
        assert!(!add_label_allowed("popup"));
        assert!(!add_label_allowed("xpopup-3"));
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
