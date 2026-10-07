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

// ---------------------------------------------------------------------------
// v0.10.0 tabbed-window E2E driver (Xvfb smoke).
//
// Driven by `APPMAKA_DEBUG_TABS_MODE=tabs` (full flow) or `=verify`
// (post-restart check). The headless E2E cannot click the native strip
// (Windows) or the HTML strip (mouse doesn't reach the webview under
// Xvfb), so this exercises the exact backend functions the strip, the
// hook, and the dashboard call: open/add/switch/close_tab/gesture-close/
// list, then the session restore path. The shell script seeds apps.json
// with two apps first.
//
// Same contract as the other flows: debug builds only, inert without the
// env var, detached thread, never blocks startup.

/// Called once from setup. Returns immediately.
pub fn maybe_run_tabs_flow(app: &AppHandle) {
    let mode = std::env::var("APPMAKA_DEBUG_TABS_MODE").unwrap_or_default();
    if mode != "tabs" && mode != "verify" {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-tabs".to_string())
        .spawn(move || {
            if mode == "verify" {
                run_tabs_verify(&app);
            } else {
                run_tabs_flow(&app);
            }
        });
}

fn run_tabs_flow(app: &AppHandle) {
    use crate::tabs;
    // Let startup, state load, and the store settle.
    std::thread::sleep(std::time::Duration::from_secs(6));

    let store = match app.try_state::<crate::store::AppStore>() {
        Some(s) => s,
        None => {
            eprintln!("[e2e] tabs: no AppStore state");
            return;
        }
    };
    let adblock = match app.try_state::<crate::adblock::AdblockState>() {
        Some(s) => s,
        None => {
            eprintln!("[e2e] tabs: no AdblockState");
            return;
        }
    };
    let tabstate = match app.try_state::<tabs::TabState>() {
        Some(s) => s,
        None => {
            eprintln!("[e2e] tabs: no TabState");
            return;
        }
    };
    let apps: Vec<String> = match store.list() {
        Ok(list) => list.iter().map(|a| a.id.clone()).collect(),
        Err(e) => {
            eprintln!("[e2e] tabs: store.list failed: {e}");
            return;
        }
    };
    eprintln!("[e2e] tabs: seeded apps: {}", apps.join(","));
    if apps.len() < 2 {
        eprintln!("[e2e] tabs: need 2 seeded apps");
        return;
    }
    let first_account = |app_id: &str| -> Option<String> {
        store
            .get(app_id)
            .ok()
            .and_then(|a| a.accounts.first().map(|ac| ac.id.clone()))
    };
    let acc0 = first_account(&apps[0]).unwrap_or_default();
    let acc1 = first_account(&apps[1]).unwrap_or_default();

    // 1. Open an empty tabbed window.
    let info = match tabs::open_tabbed_window(
        app,
        &store,
        &adblock,
        &tabstate,
        tabs::OpenTabbedParams::default(),
    ) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[e2e] tabs: open failed: {e}");
            return;
        }
    };
    let gid = info.group_id.clone();
    eprintln!(
        "[e2e] tabs: opened group {} tabs={} active={}",
        gid,
        info.tabs.len(),
        info.active
    );

    // 2. Add two tabs.
    let info = match tabs::add_tab(app, &store, &adblock, &tabstate, &gid, &apps[0], &acc0) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[e2e] tabs: add tab 0 failed: {e}");
            return;
        }
    };
    eprintln!(
        "[e2e] tabs: after add0 tabs={} active={}",
        info.tabs.len(),
        info.active
    );
    std::thread::sleep(std::time::Duration::from_secs(4));
    let info = match tabs::add_tab(app, &store, &adblock, &tabstate, &gid, &apps[1], &acc1) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[e2e] tabs: add tab 1 failed: {e}");
            return;
        }
    };
    eprintln!(
        "[e2e] tabs: after add1 tabs={} active={}",
        info.tabs.len(),
        info.active
    );
    std::thread::sleep(std::time::Duration::from_secs(4));

    // 3. Switch back to tab 0: the live window must follow.
    let info = match tabs::switch_tab(app, &store, &adblock, &tabstate, &gid, 0) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[e2e] tabs: switch failed: {e}");
            return;
        }
    };
    std::thread::sleep(std::time::Duration::from_secs(4));
    let live_url = tabs::live_tab_groups(app)
        .into_iter()
        .find(|g| g.id == gid)
        .and_then(|g| app.get_webview_window(&g.label))
        .and_then(|w| w.url().ok())
        .map(|u| u.to_string())
        .unwrap_or_default();
    eprintln!(
        "[e2e] tabs: after switch active={} live_url={}",
        info.active, live_url
    );

    // 4. Esc+LMB gesture with 2 tabs: closes the ACTIVE TAB, not the window.
    tabs::gesture_close_active_tab(app, &info_label(app, &gid));
    std::thread::sleep(std::time::Duration::from_secs(4));
    let rows = tabs::list_tabbed_windows(app);
    let g = rows.iter().find(|r| r.group_id == gid);
    eprintln!(
        "[e2e] tabs: after gesture tabs={} group_alive={}",
        g.map(|r| r.tabs.len()).unwrap_or(0),
        g.is_some()
    );

    // 5. Close the last tab: the group goes away (browser behavior).
    if let Some(g) = g {
        let _ = tabs::close_tab(app, &store, &adblock, &tabstate, &gid, g.active);
    }
    std::thread::sleep(std::time::Duration::from_secs(3));
    let alive = tabs::list_tabbed_windows(app)
        .iter()
        .any(|r| r.group_id == gid);
    eprintln!("[e2e] tabs: after last-tab close group_alive={alive}");

    // 6. Reopen with 2 tabs for the persistence round-trip: the shell
    // script kills -9 the app, relaunches with mode=verify, and the
    // session restore must bring the tab set back.
    let info = tabs::open_tabbed_window(
        app,
        &store,
        &adblock,
        &tabstate,
        tabs::OpenTabbedParams {
            initial: vec![
                tabs::TabEntry {
                    app_id: apps[0].clone(),
                    account_id: acc0.clone(),
                    last_url: None,
                },
                tabs::TabEntry {
                    app_id: apps[1].clone(),
                    account_id: acc1.clone(),
                    last_url: None,
                },
            ],
            active: 1,
            ..Default::default()
        },
    );
    match info {
        Ok(i) => {
            std::thread::sleep(std::time::Duration::from_secs(4));
            eprintln!(
                "[e2e] tabs: persist group {} tabs={} active={}",
                i.group_id,
                i.tabs.len(),
                i.active
            );
        }
        Err(e) => eprintln!("[e2e] tabs: persist open failed: {e}"),
    }
    eprintln!("[e2e] tabs: flow done");
}

/// Current live window label for a group (for the gesture entry point).
fn info_label(app: &AppHandle, group_id: &str) -> String {
    crate::tabs::live_tab_groups(app)
        .into_iter()
        .find(|g| g.id == group_id)
        .map(|g| g.label)
        .unwrap_or_default()
}

/// Post-restart: run the real session restore, then report the tabbed
/// groups. The shell script asserts the tab set + active tab survived.
fn run_tabs_verify(app: &AppHandle) {
    use crate::tabs;
    std::thread::sleep(std::time::Duration::from_secs(6));
    let n = crate::session::restore_session_now(app);
    eprintln!("[e2e] tabs: restore_session_now reopened {n}");
    std::thread::sleep(std::time::Duration::from_secs(8));
    for g in tabs::list_tabbed_windows(app) {
        let names: Vec<String> = g.tabs.iter().map(|t| t.app_name.clone()).collect();
        eprintln!(
            "[e2e] tabs: restored group tabs={} active={} names={}",
            g.tabs.len(),
            g.active,
            names.join(",")
        );
    }
    eprintln!("[e2e] tabs: verify done");
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
