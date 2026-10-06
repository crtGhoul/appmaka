//! Debug-only E2E driver for the v0.9.11 popup revamp (Xvfb smoke).
//!
//! The headless E2E has no way to click through the launcher, so this module
//! drives the popup flow directly: it opens a popup at a URL from the
//! `APPMAKA_DEBUG_POPUP_URL` env var, then — depending on
//! `APPMAKA_DEBUG_POPUP_MODE` (`close` or `add`) — exercises the exact
//! backend functions the RAM dashboard calls (`list_open_account_windows`,
//! `close_open_window`, and the add-to-applications flow) and logs their
//! results as JSON for the shell script to assert on.
//!
//! This module is compiled ONLY in debug builds (`#[cfg(debug_assertions)]`
//! in main.rs) AND does nothing unless the env var is set, so release
//! builds never contain this path. Everything runs on a detached thread;
//! startup is never blocked.

use tauri::{AppHandle, Manager};

/// Called once from setup. Returns immediately.
pub fn maybe_run_popup_flow(app: &AppHandle) {
    let url = std::env::var("APPMAKA_DEBUG_POPUP_URL").unwrap_or_default();
    if url.is_empty() {
        return;
    }
    let mode = std::env::var("APPMAKA_DEBUG_POPUP_MODE").unwrap_or_default();
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-e2e".to_string())
        .spawn(move || run_flow(&app, &url, &mode));
}

fn run_flow(app: &AppHandle, url: &str, mode: &str) {
    // Give the app time to finish starting up before opening anything.
    std::thread::sleep(std::time::Duration::from_secs(5));
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[e2e] bad APPMAKA_DEBUG_POPUP_URL: {e}");
            return;
        }
    };
    // Popups share their parent's session dir in production; the E2E has no
    // parent, so it uses a throwaway dir. The add flow must still create a
    // FRESH dir for the new app — that invariant is what the test watches.
    let session_dir = std::env::temp_dir().join("appmaka-e2e-popup-session");
    let _ = std::fs::create_dir_all(&session_dir);
    crate::windows::spawn_popup_window(app, &parsed, "E2E Test", &session_dir);
    eprintln!("[e2e] popup requested for {url}");
    // Let the window build and the navigation settle.
    std::thread::sleep(std::time::Duration::from_secs(10));
    log_rows(app, "after-open");

    match mode {
        "add" => {
            let Some(label) = find_popup_label(app) else {
                eprintln!("[e2e] add mode: no popup row found");
                return;
            };
            let store = app.state::<crate::store::AppStore>();
            let adblock = app.state::<crate::adblock::AdblockState>();
            let winstate = app.state::<crate::windows::WindowState>();
            match crate::popup_add::run_add(app, &store, &adblock, &winstate, &label) {
                Ok(outcome) => match serde_json::to_string(&outcome) {
                    Ok(json) => eprintln!("[e2e] add outcome: {json}"),
                    Err(e) => eprintln!("[e2e] add outcome unserializable: {e}"),
                },
                Err(e) => eprintln!("[e2e] add failed: {e}"),
            }
            // Let the new app window open and the popup close.
            std::thread::sleep(std::time::Duration::from_secs(8));
            log_rows(app, "after-add");
        }
        "close" => {
            let Some(label) = find_popup_label(app) else {
                eprintln!("[e2e] close mode: no popup row found");
                return;
            };
            match crate::windows::close_open_window(app.clone(), label.clone()) {
                Ok(()) => eprintln!("[e2e] closed {label}"),
                Err(e) => eprintln!("[e2e] close failed: {e}"),
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
            log_rows(app, "after-close");
        }
        _ => {}
    }
    // The shell script screenshots and kills the process; nothing to do.
}

/// The exact rows the RAM dashboard renders, as JSON.
fn log_rows(app: &AppHandle, stage: &str) {
    let store = app.state::<crate::store::AppStore>();
    let rows = crate::windows::list_open_account_windows(app.clone(), store);
    match serde_json::to_string(&rows) {
        Ok(json) => eprintln!("[e2e] rows {stage}: {json}"),
        Err(e) => eprintln!("[e2e] rows {stage} unserializable: {e}"),
    }
}

fn find_popup_label(app: &AppHandle) -> Option<String> {
    let store = app.state::<crate::store::AppStore>();
    crate::windows::list_open_account_windows(app.clone(), store)
        .into_iter()
        .find(|r| r.label.starts_with("popup-"))
        .map(|r| r.label)
}

// ---------------------------------------------------------------------------
// v0.9.12 clipboard pinning E2E driver (Xvfb smoke).
//
// Driven by `APPMAKA_DEBUG_CLIPBOARD_MODE=pin`. The headless E2E cannot
// click the popup's pin buttons (mouse clicks don't reach the
// WebKitGTK webview under Xvfb), so this exercises the exact backend
// commands the popup calls (`list_clipboard`, `set_clipboard_pinned`,
// `set_clipboard_pinned_only`, `get_clipboard_settings`) and logs
// `[e2e]` lines for the shell script to assert on. The shell script
// seeds clipboard.json first, so the load path (legacy entries without
// the `pinned` field included) is covered too.
//
// Same contract as the popup flow above: debug builds only, inert
// without the env var, detached thread, never blocks startup.

/// Called once from setup. Returns immediately.
pub fn maybe_run_clipboard_flow(app: &AppHandle) {
    if std::env::var("APPMAKA_DEBUG_CLIPBOARD_MODE").unwrap_or_default() != "pin" {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-clipboard".to_string())
        .spawn(move || run_clipboard_flow(&app));
}

fn run_clipboard_flow(app: &AppHandle) {
    use crate::clipboard;
    // Let startup, state load, and the watcher settle.
    std::thread::sleep(std::time::Duration::from_secs(6));

    let entries = match clipboard::list_clipboard(app.clone()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[e2e] list_clipboard failed: {e}");
            return;
        }
    };
    eprintln!("[e2e] entries: {}", entries.len());
    let pinned_ids: Vec<&str> = entries
        .iter()
        .filter(|e| e.pinned)
        .map(|e| e.id.as_str())
        .collect();
    eprintln!("[e2e] pinned on load: {}", pinned_ids.join(","));

    // Pin the first unpinned entry, verify it reads back pinned, then
    // unpin it again and verify that too.
    if let Some(target) = entries.iter().find(|e| !e.pinned) {
        let id = target.id.clone();
        match clipboard::set_clipboard_pinned(app.clone(), id.clone(), true) {
            Ok(v) => eprintln!("[e2e] set pinned=true -> {v}"),
            Err(e) => eprintln!("[e2e] set pinned=true failed: {e}"),
        }
        let now_pinned = clipboard::list_clipboard(app.clone())
            .unwrap_or_default()
            .into_iter()
            .find(|e| e.id == id)
            .map(|e| e.pinned)
            .unwrap_or(false);
        eprintln!("[e2e] pinned after set: {now_pinned}");

        let _ = clipboard::set_clipboard_pinned(app.clone(), id.clone(), false);
        let now_clear = clipboard::list_clipboard(app.clone())
            .unwrap_or_default()
            .into_iter()
            .find(|e| e.id == id)
            .map(|e| !e.pinned)
            .unwrap_or(false);
        eprintln!("[e2e] unpinned after clear: {now_clear}");
    } else {
        eprintln!("[e2e] no unpinned entry to exercise");
    }

    // The "Pinned only" switch round-trips through settings.
    match clipboard::set_clipboard_pinned_only(app.clone(), true) {
        Ok(v) => eprintln!("[e2e] set pinned_only=true -> {v}"),
        Err(e) => eprintln!("[e2e] set pinned_only failed: {e}"),
    }
    let back = clipboard::get_clipboard_settings(app.clone())
        .map(|s| s.popup_pinned_only)
        .unwrap_or(false);
    eprintln!("[e2e] settings pinned_only: {back}");
    let _ = clipboard::set_clipboard_pinned_only(app.clone(), false);
    eprintln!("[e2e] clipboard flow done");
}
