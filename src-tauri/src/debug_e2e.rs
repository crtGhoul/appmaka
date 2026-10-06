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
