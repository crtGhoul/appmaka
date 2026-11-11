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
                    page_title: String::new(),
                    page_title_seen: false,
                },
                tabs::TabEntry {
                    app_id: apps[1].clone(),
                    account_id: acc1.clone(),
                    last_url: None,
                    page_title: String::new(),
                    page_title_seen: false,
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
    let ctx = crate::windows::PopupContext {
        app: app.clone(),
        app_id: "e2e".to_string(),
        account_id: "e2e".to_string(),
        app_name: "E2E Test".to_string(),
        app_url: parsed.to_string(),
        session_dir: session_dir.clone(),
        popup_policy: "allow".to_string(),
        popup_allowlist: Vec::new(),
        parent_label: String::new(),
        chain_allowed: false,
    };
    crate::windows::spawn_popup_window(&ctx, &parsed, None);
    eprintln!("[e2e] popup requested for {url}");
    // Let the window build and the navigation settle.
    std::thread::sleep(std::time::Duration::from_secs(10));
    log_rows(app, "after-open");

    // v0.18.7: the popup bar window should be docked under the popup.
    let bar_present = find_popup_label(app)
        .and_then(|l| app.get_webview_window(&format!("popupbar-{l}")))
        .is_some();
    eprintln!(
        "[e2e] popup-bar: {}",
        if bar_present { "present" } else { "missing" }
    );

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
            match tauri::async_runtime::block_on(crate::windows::close_open_window(
                app.clone(),
                label.clone(),
            )) {
                Ok(()) => eprintln!("[e2e] closed {label}"),
                Err(e) => eprintln!("[e2e] close failed: {e}"),
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
            log_rows(app, "after-close");
        }
        _ => {}
    }
    // The shell script screenshots and kills the process; nothing to do.
    // Signal completion so the script's wait loop exits promptly instead
    // of burning its full timeout (it greps for "flow done").
    eprintln!("[e2e] popup: flow done");
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
// v0.18.8 split view E2E driver (Xvfb smoke).
//
// Driven by `APPMAKA_DEBUG_SPLIT_MODE=split`. The shell script seeds four
// apps (apps.json); this opens one account standalone first (pull proof),
// then a four-pane split in ONE frame window, verifies the frame, the
// bar webview, and the four live child panes, removes one pane and
// verifies the survivors reflow to the three-pane layout, then removes
// down to dissolve and verifies the last pane reopens as an ordinary
// window. Logs `[e2e]` lines for the shell script to assert on.
//
// NOTE (Linux): the panes are NOT tiled here — Tauri's Linux runtime
// packs child webviews into the frame's vertical box and ignores bounds,
// so the probes report full-width equal-height strips (1280x160 at four
// panes) instead of 640x382 tiles. The state machine (open/pull/remove/
// reflow/dissolve/restore) is what this driver proves on Linux; the
// tiling itself is a Windows/macOS behavior, gated on the PC.
//
// Same contract as the tabs flow above: debug builds only, inert without
// the env var, detached thread, never blocks startup.
// ---------------------------------------------------------------------------

/// Called once from setup. Returns immediately.
pub fn maybe_run_split_flow(app: &AppHandle) {
    let mode = std::env::var("APPMAKA_DEBUG_SPLIT_MODE").unwrap_or_default();
    if mode != "split" {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-split".to_string())
        .spawn(move || run_split_flow(&app));
}

/// Read a pane webview's inner size via JS (best-effort: the callback
/// may never fire under Xvfb, so it times out instead of hanging).
fn pane_inner_size(w: &tauri::Webview) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = w.eval_with_callback(
        "JSON.stringify({w: window.innerWidth, h: window.innerHeight})",
        move |v| {
            let _ = tx.send(v.to_string());
        },
    );
    rx.recv_timeout(std::time::Duration::from_secs(3)).ok()
}

fn run_split_flow(app: &AppHandle) {
    use crate::split;
    // Let startup, state load, and the store settle.
    std::thread::sleep(std::time::Duration::from_secs(6));

    let store = match app.try_state::<crate::store::AppStore>() {
        Some(s) => s,
        None => {
            eprintln!("[e2e] split: no AppStore state");
            return;
        }
    };
    let adblock = match app.try_state::<crate::adblock::AdblockState>() {
        Some(s) => s,
        None => {
            eprintln!("[e2e] split: no AdblockState");
            return;
        }
    };
    let winstate = match app.try_state::<crate::windows::WindowState>() {
        Some(s) => s,
        None => {
            eprintln!("[e2e] split: no WindowState");
            return;
        }
    };
    let splitstate = match app.try_state::<split::SplitState>() {
        Some(s) => s,
        None => {
            eprintln!("[e2e] split: no SplitState");
            return;
        }
    };
    let apps: Vec<String> = match store.list() {
        Ok(list) => list.iter().map(|a| a.id.clone()).collect(),
        Err(e) => {
            eprintln!("[e2e] split: store.list failed: {e}");
            return;
        }
    };
    eprintln!("[e2e] split: seeded apps: {}", apps.join(","));
    if apps.len() < 4 {
        eprintln!("[e2e] split: need 4 seeded apps");
        return;
    }
    let first_account = |app_id: &str| -> Option<String> {
        store
            .get(app_id)
            .ok()
            .and_then(|a| a.accounts.first().map(|ac| ac.id.clone()))
    };
    let panes: Vec<split::SplitPane> = apps
        .iter()
        .take(4)
        .map(|id| split::SplitPane {
            app_id: id.clone(),
            account_id: first_account(id).unwrap_or_default(),
        })
        .collect();

    // 0. Pull proof: open the first account standalone, then open the
    // split including it — the standalone window must be gone, replaced
    // by a child webview on the same session.
    let first = &panes[0];
    let first_label = crate::windows::account_window_label(&first.app_id, &first.account_id);
    match crate::windows::open_account_placed(
        app, &store, &adblock, &winstate, &first.app_id, &first.account_id, None, false, false,
    ) {
        Ok(()) => eprintln!("[e2e] split: standalone {} opened", first.app_id),
        Err(e) => {
            eprintln!("[e2e] split: standalone open failed: {e}");
            return;
        }
    }
    std::thread::sleep(std::time::Duration::from_secs(3));
    eprintln!(
        "[e2e] split: standalone window {}",
        if app.get_webview_window(&first_label).is_some() {
            "present"
        } else {
            "missing"
        }
    );

    // 1. Open a four-pane split (layout defaults from the count).
    let info = match split::open_split(app, &store, &adblock, &winstate, &splitstate, panes, None)
    {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[e2e] split: open failed: {e}");
            return;
        }
    };
    let gid = info.id.clone();
    eprintln!(
        "[e2e] split: opened group {} panes={} layout={:?}",
        gid,
        info.panes.len(),
        info.layout
    );
    std::thread::sleep(std::time::Duration::from_secs(5));

    // 2. ONE frame window exists, the bar webview is on it, and all four
    // pane webviews are live — no standalone windows for the split's
    // accounts (the pulled one included).
    match app.get_window(&split::split_frame_label(&gid)) {
        Some(f) => match f.inner_size() {
            Ok(s) => eprintln!("[e2e] split: frame present {}x{}", s.width, s.height),
            Err(_) => eprintln!("[e2e] split: frame present (size unreadable)"),
        },
        None => eprintln!("[e2e] split: frame missing"),
    }
    eprintln!(
        "[e2e] split: bar {}",
        if app
            .get_webview(&format!("splitbar-{gid}"))
            .is_some()
        {
            "present"
        } else {
            "missing"
        }
    );
    let mut live = 0;
    for pane in &info.panes {
        let label = split::pane_webview_label(&gid, &pane.app_id, &pane.account_id);
        if let Some(w) = app.get_webview(&label) {
            live += 1;
            match pane_inner_size(&w) {
                Some(s) => eprintln!("[e2e] split: pane {} inner {s}", pane.app_id),
                None => eprintln!("[e2e] split: pane {} inner unreadable", pane.app_id),
            }
        }
        let standalone = crate::windows::account_window_label(&pane.app_id, &pane.account_id);
        if app.get_webview_window(&standalone).is_some() {
            eprintln!("[e2e] split: pane {} still has a standalone window", pane.app_id);
        }
    }
    eprintln!("[e2e] split: live panes {live}/4");

    // 3. Remove one pane: the survivors reflow to the three-pane layout
    // (left tall 640x764, right two 640x382 each in a 1280x800 frame on
    // Windows/macOS; on Linux the probes show the stacked 1280x200
    // strips instead — same state, runtime layout difference).
    let info = match split::remove_split_pane(app, &store, &adblock, &winstate, &splitstate, &gid, 3) {
        Ok(Some(i)) => i,
        Ok(None) => {
            eprintln!("[e2e] split: group dissolved after one removal (expected reflow)");
            return;
        }
        Err(e) => {
            eprintln!("[e2e] split: remove failed: {e}");
            return;
        }
    };
    eprintln!(
        "[e2e] split: after remove panes={} layout={:?}",
        info.panes.len(),
        info.layout
    );
    std::thread::sleep(std::time::Duration::from_secs(3));
    let mut live = 0;
    for pane in &info.panes {
        let label = split::pane_webview_label(&gid, &pane.app_id, &pane.account_id);
        if let Some(w) = app.get_webview(&label) {
            live += 1;
            match pane_inner_size(&w) {
                Some(s) => eprintln!("[e2e] split: reflowed pane {} inner {s}", pane.app_id),
                None => eprintln!("[e2e] split: reflowed pane {} inner unreadable", pane.app_id),
            }
        }
    }
    eprintln!("[e2e] split: live panes after remove {live}/3");
    eprintln!(
        "[e2e] split: frame {}",
        if app.get_window(&split::split_frame_label(&gid)).is_some() {
            "present"
        } else {
            "missing"
        }
    );

    // 3.5 v0.18.10: drag-swap — move the first pane to the end. The
    // active pane follows its account, not its index.
    let first_before = info.panes[0].clone();
    let info = match split::move_split_pane(app, &splitstate, &gid, 0, 2) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[e2e] split: move failed: {e}");
            return;
        }
    };
    let move_ok = info
        .panes
        .get(2)
        .map(|p| p.app_id == first_before.app_id && p.account_id == first_before.account_id)
        .unwrap_or(false)
        && info
            .panes
            .get(info.active_pane)
            .map(|p| p.app_id == first_before.app_id)
            .unwrap_or(false);
    eprintln!(
        "[e2e] split: move order_ok={move_ok} active={}",
        info.active_pane
    );

    // 3.6 v0.18.10: a gesture inside a page marks its pane active — but
    // only a REAL gesture. A synthetic pointerdown is ignored by design
    // (hardened v0.18.9: the gesture reporter drops isTrusted === false),
    // so the active pane must NOT move on the dispatch below. The real
    // path is then proven the way a user drives it: a direct
    // `note_user_gesture` invoke from pane 0's webview, which must move
    // the highlight to pane 0.
    let active_pane = || {
        split::list_splits(app)
            .map(|l| {
                l.open
                    .iter()
                    .find(|g| g.id == gid)
                    .map(|g| g.active_pane)
                    .unwrap_or(usize::MAX)
            })
            .unwrap_or(usize::MAX)
    };
    let pane0 = app.get_webview(&split::pane_webview_label(
        &gid,
        &info.panes[0].app_id,
        &info.panes[0].account_id,
    ));
    if let Some(w) = &pane0 {
        let _ = eval_in(
            w,
            "document.dispatchEvent(new PointerEvent('pointerdown', {bubbles: true})); 'dispatched'",
        );
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    eprintln!(
        "[e2e] split: synthetic_gesture_ignored active_still={}",
        active_pane()
    );
    if let Some(w) = &pane0 {
        let _ = eval_in(
            w,
            "window.__invokeResult = 'pending'; window.__TAURI_INTERNALS__.invoke('note_user_gesture', {}).then(function(){ window.__invokeResult = 'ok'; }).catch(function(e){ window.__invokeResult = 'err:' + e; }); 'sent'",
        );
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    let invoke_result = pane0
        .as_ref()
        .and_then(|w| eval_in(w, "window.__invokeResult"))
        .unwrap_or_default();
    eprintln!(
        "[e2e] split: direct_gesture_active={} invokeResult={invoke_result}",
        active_pane()
    );
    // 3.6b: focus transfer — focusing pane 1 moves the active pane and
    // focuses the frame + the pane webview (the open_split focus path).
    // Real mouse focus is PC-gated; this proves the backend transfer.
    match split::focus_split_pane(app, &splitstate, &gid, 1) {
        Ok(()) => eprintln!(
            "[e2e] split: focus_transfer pane=1 active={} ok=true",
            active_pane()
        ),
        Err(e) => eprintln!("[e2e] split: focus_transfer pane=1 ok=false err={e}"),
    }

    // 3.7 v0.18.10: add the removed app back — pull its standalone
    // window into the open split. The app removed in step 3 is the one
    // missing from the pane list.
    let removed_id = apps
        .iter()
        .find(|id| !info.panes.iter().any(|p| &p.app_id == *id))
        .cloned()
        .unwrap_or_default();
    let removed_acct = format!("{removed_id}-a");
    if removed_id.is_empty() {
        eprintln!("[e2e] split: add skipped (no removed app found)");
        return;
    }
    if let Err(e) = crate::windows::open_account_placed(
        app, &store, &adblock, &winstate, &removed_id, &removed_acct, None, false, false,
    ) {
        eprintln!("[e2e] split: standalone reopen failed: {e}");
        return;
    }
    std::thread::sleep(std::time::Duration::from_secs(3));
    let standalone_label = crate::windows::account_window_label(&removed_id, &removed_acct);
    let standalone_before = app.get_webview_window(&standalone_label).is_some();
    let info = match split::add_pane_to_split(
        app, &store, &adblock, &winstate, &splitstate, &gid, &removed_id, &removed_acct,
    ) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[e2e] split: add failed: {e}");
            return;
        }
    };
    let pane_label = split::pane_webview_label(&gid, &removed_id, &removed_acct);
    eprintln!(
        "[e2e] split: add_pullin panes={} standalone_before={standalone_before} standalone_after={} pane={}",
        info.panes.len(),
        app.get_webview_window(&standalone_label).is_some(),
        app.get_webview(&pane_label).is_some()
    );

    // 3.8 v0.18.10: pop the just-added pane back out. The group returns
    // to three panes and the popped app has its own window again — the
    // exact state step 4 expects.
    let info = match split::pop_out_split_pane(app, &store, &adblock, &winstate, &splitstate, &gid, 3) {
        Ok(Some(i)) => i,
        Ok(None) => {
            eprintln!("[e2e] split: popout dissolved the group (expected a group)");
            return;
        }
        Err(e) => {
            eprintln!("[e2e] split: popout failed: {e}");
            return;
        }
    };
    eprintln!(
        "[e2e] split: popout panes={} standalone={}",
        info.panes.len(),
        app.get_webview_window(&standalone_label).is_some()
    );

    // 4. Remove down to dissolve: the last pane reopens as an ordinary
    // standalone window and the frame closes.
    let last = match info.panes.last() {
        Some(p) => p.clone(),
        None => {
            eprintln!("[e2e] split: no panes left to dissolve");
            return;
        }
    };
    let last_label = crate::windows::account_window_label(&last.app_id, &last.account_id);
    match split::remove_split_pane(app, &store, &adblock, &winstate, &splitstate, &gid, 1) {
        Ok(Some(i)) => eprintln!("[e2e] split: after second remove panes={}", i.panes.len()),
        Ok(None) => {
            eprintln!("[e2e] split: group dissolved early (expected two panes)");
            return;
        }
        Err(e) => {
            eprintln!("[e2e] split: second remove failed: {e}");
            return;
        }
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    // v0.18.10: the dissolving survivor reopens where it was. Navigate
    // its pane somewhere other than home first, then check the
    // standalone window lands there.
    let mut dissolve_target = String::new();
    if let Some(w) = app.get_webview(&split::pane_webview_label(
        &gid,
        &last.app_id,
        &last.account_id,
    )) {
        let current = w.url().map(|u| u.to_string()).unwrap_or_default();
        let target = if current.contains("example.com") {
            "https://example.org"
        } else {
            "https://example.com"
        };
        let _ = eval_in(&w, &format!("window.location.href='{target}'; 'navigated'"));
        std::thread::sleep(std::time::Duration::from_secs(4));
        dissolve_target = w.url().map(|u| u.to_string()).unwrap_or_default();
        eprintln!("[e2e] split: survivor pane now at {dissolve_target}");
    }
    match split::remove_split_pane(app, &store, &adblock, &winstate, &splitstate, &gid, 0) {
        Ok(None) => eprintln!("[e2e] split: group dissolved"),
        Ok(Some(_)) => {
            eprintln!("[e2e] split: group did not dissolve (expected dissolve)");
            return;
        }
        Err(e) => {
            eprintln!("[e2e] split: dissolve remove failed: {e}");
            return;
        }
    }
    // The dissolve rebuilds the last pane as a standalone window; give
    // the builder thread time, then check both halves of the outcome.
    std::thread::sleep(std::time::Duration::from_secs(8));
    eprintln!(
        "[e2e] split: frame after dissolve {}",
        if app.get_window(&split::split_frame_label(&gid)).is_some() {
            "present (expected gone)"
        } else {
            "gone"
        }
    );
    eprintln!(
        "[e2e] split: last pane standalone {}",
        if app.get_webview_window(&last_label).is_some() {
            "present"
        } else {
            "missing"
        }
    );
    // v0.18.10: the survivor reopens where it was (step 4's navigation),
    // not on its home page.
    let standalone_url = app
        .get_webview_window(&last_label)
        .and_then(|w| w.url().ok())
        .map(|u| u.to_string())
        .unwrap_or_default();
    eprintln!(
        "[e2e] split: dissolve_url target={dissolve_target} standalone={standalone_url}"
    );

    // 5. v0.18.10: close-all closes standalone windows AND split
    // groups. Rebuild one standalone plus a two-pane split, then count
    // what close-all reports against what was live.
    let others: Vec<String> = apps
        .iter()
        .filter(|id| **id != last.app_id)
        .cloned()
        .collect();
    if others.len() >= 3 {
        let _ = crate::windows::open_account_placed(
            app,
            &store,
            &adblock,
            &winstate,
            &others[0],
            &format!("{}-a", others[0]),
            None,
            false,
            false,
        );
        std::thread::sleep(std::time::Duration::from_secs(3));
        let split_panes: Vec<split::SplitPane> = others[1..3]
            .iter()
            .map(|id| split::SplitPane {
                app_id: id.clone(),
                account_id: format!("{id}-a"),
            })
            .collect();
        match split::open_split(app, &store, &adblock, &winstate, &splitstate, split_panes, None)
        {
            Ok(i) => eprintln!("[e2e] split: closeall setup group {} panes={}", i.id, i.panes.len()),
            Err(e) => eprintln!("[e2e] split: closeall setup failed: {e}"),
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
        let live_acct = app
            .webview_windows()
            .keys()
            .filter(|l| l.starts_with("acct-"))
            .count();
        let live_panes = splitstate.total_panes();
        let closed =
            tauri::async_runtime::block_on(crate::windows::close_all_account_windows(app.clone()));
        // Window destruction lands on the event loop: give WebKitGTK a
        // beat before counting, or a window mid-destroy reads as alive.
        std::thread::sleep(std::time::Duration::from_secs(2));
        let remaining_labels: Vec<String> = app
            .webview_windows()
            .keys()
            .filter(|l| l.starts_with("acct-"))
            .cloned()
            .collect();
        let remaining_acct = remaining_labels.len();
        eprintln!(
            "[e2e] split: closeall expected={} closed={closed:?} remaining_acct={remaining_acct} remaining_panes={} remaining_labels={remaining_labels:?}",
            live_acct + live_panes,
            splitstate.total_panes()
        );
    }

    // 6. v0.18.10: idle panes shed memory. The seed sets the example
    // apps' auto_suspend_minutes to 1; the watcher ticks every 60s, so
    // a pane sheds on the second tick after it opens. Focus the launcher
    // first (a focused frame never sheds), then wait out two ticks.
    let shed_panes: Vec<split::SplitPane> = apps[..2]
        .iter()
        .map(|id| split::SplitPane {
            app_id: id.clone(),
            account_id: format!("{id}-a"),
        })
        .collect();
    match split::open_split(app, &store, &adblock, &winstate, &splitstate, shed_panes, None) {
        Ok(i) => eprintln!("[e2e] split: shed setup group {} panes={}", i.id, i.panes.len()),
        Err(e) => eprintln!("[e2e] split: shed setup failed: {e}"),
    }
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.set_focus();
    }
    std::thread::sleep(std::time::Duration::from_secs(85));
    eprintln!("[e2e] split: shed_wait done");
    eprintln!("[e2e] split: flow done");
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

// ---------------------------------------------------------------------------
// v0.11.0 error-dialog + tint E2E drivers (Xvfb smoke).
//
// APPMAKA_DEBUG_ERROR_MODE=error: records a synthetic backend error with
// notify=true (the dialog path) and logs the ring-buffer entry. The dialog
// itself is frontend; the entry proves the backend half (record +
// sanitize + the emit call, which would panic on a bad payload).
//
// APPMAKA_DEBUG_TINT_MODE=tint: opens two seeded page windows at local
// test pages (APPMAKA_DEBUG_TINT_BASE=http://127.0.0.1:PORT) — one with a
// theme-color meta tag, one with only a body background — runs the full
// tint::resolve_now chain against each, and logs the decisions. This
// exercises the fallback chain headless (HTTP fetch + live-DOM eval under
// WebKitGTK); DWM application itself is Windows-only and stays logic-only.
//
// Same contract as the other flows: debug builds only, inert without the
// env vars, detached threads, never blocks startup.
/// Called once from setup. Returns immediately.
pub fn maybe_run_error_flow(app: &AppHandle) {
    if std::env::var("APPMAKA_DEBUG_ERROR_MODE").unwrap_or_default() != "error" {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-error".to_string())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(6));
            let id = crate::errors::record(
                &app,
                "e2e-test",
                "Synthetic E2E failure.",
                "technical: forced by APPMAKA_DEBUG_ERROR_MODE; token=should-be-redacted",
                true,
            );
            let entries = crate::errors::get_recent_errors(app.clone());
            match entries.iter().find(|e| e.id == id) {
                Some(e) => eprintln!(
                    "[e2e] error: id={} kind={} entries={} redacted={}",
                    e.id,
                    e.kind,
                    entries.len(),
                    !e.technical.contains("should-be-redacted")
                ),
                None => eprintln!("[e2e] error: ENTRY MISSING id={id}"),
            }
            eprintln!("[e2e] error: flow done");
        });
}

/// Called once from setup. Returns immediately.
pub fn maybe_run_tint_flow(app: &AppHandle) {
    if std::env::var("APPMAKA_DEBUG_TINT_MODE").unwrap_or_default() != "tint" {
        return;
    }
    let base = std::env::var("APPMAKA_DEBUG_TINT_BASE").unwrap_or_default();
    if base.is_empty() {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-tint".to_string())
        .spawn(move || run_tint_flow(&app, &base));
}

fn run_tint_flow(app: &AppHandle, base: &str) {
    use std::time::Duration;
    std::thread::sleep(Duration::from_secs(6));
    let (Some(store), Some(adblock), Some(winstate)) = (
        app.try_state::<crate::store::AppStore>(),
        app.try_state::<crate::adblock::AdblockState>(),
        app.try_state::<crate::windows::WindowState>(),
    ) else {
        eprintln!("[e2e] tint: missing state");
        return;
    };
    for (app_id, acc_id, page, want_source, want_rgb) in [
        ("app-meta", "acc-meta", "meta.html", "theme-meta-http", "#123456"),
        ("app-bg", "acc-bg", "bg.html", "page-background", "#0a141e"),
    ] {
        let url = format!("{base}/{page}");
        if let Err(e) =
            crate::windows::open_account(app, &store, &adblock, &winstate, app_id, acc_id, false)
        {
            eprintln!("[e2e] tint: open {page} failed: {e}");
            continue;
        }
        let label = crate::windows::account_window_label(app_id, acc_id);
        // Let the page load before resolving (the fast HTTP path needs no
        // DOM, but the bg fallback's eval does).
        std::thread::sleep(Duration::from_secs(8));
        let d = crate::tint::resolve_now(app, &label, &url);
        let rgb = d
            .rgb
            .map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}"))
            .unwrap_or_else(|| "default".to_string());
        let src = match d.source {
            crate::tint::TintSource::ThemeMetaHttp => "theme-meta-http",
            crate::tint::TintSource::ThemeMetaDom => "theme-meta-dom",
            crate::tint::TintSource::PageBackground => "page-background",
            crate::tint::TintSource::Default => "default",
        };
        eprintln!(
            "[e2e] tint: page={page} source={src} rgb={rgb} want_source={want_source} want_rgb={want_rgb}"
        );
    }
    eprintln!("[e2e] tint: flow done");
}

// Driven by `APPMAKA_DEBUG_SECURITY_MODE=security` plus
// `APPMAKA_DEBUG_SECURITY_BASE=http://127.0.0.1:PORT` and
// `APPMAKA_DEBUG_SECURITY_DL_DIR=<download dir>`. The shell script seeds
// one "sec" app at {base}/sec.html (a page with a download link) and a
// downloads.json pointing at the dl dir, then this proves the 0.18.9
// popup/download blocking:
//   1. the window.open guard is injected and a no-gesture window.open
//      returns null;
//   2. a download triggered with no gesture is blocked (no file lands);
//   3. after a dispatched pointerdown (the gesture script reports it),
//      the same download is allowed (the file lands).
// Logs `[e2e] security: ...` lines for the shell script to assert on.
// Same contract as the other flows: debug builds only, inert without the
// env vars, detached thread, never blocks startup.
pub fn maybe_run_security_flow(app: &AppHandle) {
    if std::env::var("APPMAKA_DEBUG_SECURITY_MODE").unwrap_or_default() != "security" {
        return;
    }
    let base = std::env::var("APPMAKA_DEBUG_SECURITY_BASE").unwrap_or_default();
    let dl_dir = std::env::var("APPMAKA_DEBUG_SECURITY_DL_DIR").unwrap_or_default();
    if base.is_empty() || dl_dir.is_empty() {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-security".to_string())
        .spawn(move || run_security_flow(&app, &dl_dir));
}

/// Eval JS in a webview and return the callback value (best-effort, 3s
/// timeout — the callback may never fire under Xvfb).
fn eval_in(w: &tauri::Webview, js: &str) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = w.eval_with_callback(js, move |v| {
        let _ = tx.send(v.to_string());
    });
    rx.recv_timeout(std::time::Duration::from_secs(3)).ok()
}

fn run_security_flow(app: &AppHandle, dl_dir: &str) {
    use std::time::Duration;
    std::thread::sleep(Duration::from_secs(6));
    // v0.18.9 regression check: the main window must get REAL backend data
    // through the frontend shim, not the browser mock. If the ACL denies
    // list_apps, the shim silently falls back to its canned localStorage
    // data (empty in a fresh test profile), so a 0 here means the mock is
    // active even though the window looks alive. The shell script seeds 5
    // apps and asserts list_apps=5.
    if let Some(main) = app.get_webview("main") {
        let _ = eval_in(
            &main,
            "window.__e2eMainList = 'pending'; window.__APPMAKA_INTERNALS__.invoke('list_apps', {}).then(function(a){ window.__e2eMainList = 'ok:' + a.length; }).catch(function(e){ window.__e2eMainList = 'err:' + e; }); 'sent'",
        );
        std::thread::sleep(Duration::from_secs(2));
        let main_list = eval_in(&main, "window.__e2eMainList").unwrap_or_default();
        eprintln!("[e2e] main: list_apps={main_list}");
    } else {
        eprintln!("[e2e] main: list_apps=no-main-webview");
    }
    let (Some(store), Some(adblock), Some(winstate)) = (
        app.try_state::<crate::store::AppStore>(),
        app.try_state::<crate::adblock::AdblockState>(),
        app.try_state::<crate::windows::WindowState>(),
    ) else {
        eprintln!("[e2e] security: missing state");
        return;
    };
    if let Err(e) =
        crate::windows::open_account(app, &store, &adblock, &winstate, "sec", "sec-a", false)
    {
        eprintln!("[e2e] security: open failed: {e}");
        return;
    }
    let label = crate::windows::account_window_label("sec", "sec-a");
    std::thread::sleep(Duration::from_secs(8));
    let Some(wv) = app.get_webview(&label) else {
        eprintln!("[e2e] security: no webview for {label}");
        return;
    };
    // 1. Guard injected, and a no-gesture window.open returns null.
    let guard = eval_in(
        &wv,
        "window.open.toString().indexOf('userActivation') !== -1",
    )
    .unwrap_or_default();
    let open_null = eval_in(&wv, "window.open('https://example.com') === null")
        .unwrap_or_default();
    eprintln!("[e2e] security: guard={guard} openNull={open_null}");
    // Diagnostics for the gesture transport: is the internals object
    // present, is invoke a function, and does a dispatched pointerdown
    // reach a document capture listener at all?
    let internals = eval_in(&wv, "typeof window.__TAURI_INTERNALS__").unwrap_or_default();
    let invoke_fn = eval_in(
        &wv,
        "typeof (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke)",
    )
    .unwrap_or_default();
    let pointer_event = eval_in(&wv, "typeof PointerEvent").unwrap_or_default();
    eprintln!("[e2e] security: internals={internals} invokeFn={invoke_fn} pointerEvent={pointer_event}");
    // 2. No-gesture download is blocked: the file must not land.
    let file = std::path::Path::new(dl_dir).join("file.txt");
    let _ = eval_in(&wv, "document.getElementById('dl').click(); 'clicked'");
    std::thread::sleep(Duration::from_secs(3));
    let blocked = !file.exists();
    eprintln!("[e2e] security: no-gesture download blocked={blocked}");
    // 3. A synthetic pointerdown must NOT arm the gate (hardened v0.18.9:
    // isTrusted === false is ignored by design), so the download stays
    // blocked. The direct invoke below proves the backend allow path.
    let _ = eval_in(
        &wv,
        "document.dispatchEvent(new PointerEvent('pointerdown', {bubbles: true, cancelable: true})); 'dispatched'",
    );
    std::thread::sleep(Duration::from_secs(1));
    let recorded = app
        .try_state::<crate::downloads::GestureState>()
        .map(|g| g.recent(&label, Duration::from_secs(10)))
        .unwrap_or(false);
    // The script's marker tells a fired listener apart from a failed invoke.
    let listener_fired = eval_in(
        &wv,
        "typeof window.__appmakaGestureLast !== 'undefined' && window.__appmakaGestureLast > 0",
    )
    .unwrap_or_default();
    eprintln!("[e2e] security: synthetic gesture recorded={recorded} listenerFired={listener_fired}");
    let _ = eval_in(&wv, "document.getElementById('dl').click(); 'clicked'");
    std::thread::sleep(Duration::from_secs(4));
    let allowed = file.exists();
    eprintln!("[e2e] security: synthetic-gesture download allowed={allowed}");
    // Direct invoke, bypassing the listener: if this records, the listener
    // is the broken link; if not, the invoke/command is. Capture the
    // promise outcome on the window so the exact error is visible.
    let _ = eval_in(
        &wv,
        "window.__invokeResult = 'pending'; window.__TAURI_INTERNALS__.invoke('note_user_gesture', {}).then(function(){ window.__invokeResult = 'ok'; }).catch(function(e){ window.__invokeResult = 'err:' + e; }); 'sent'",
    );
    std::thread::sleep(Duration::from_secs(2));
    let invoke_result = eval_in(&wv, "window.__invokeResult").unwrap_or_default();
    let direct = app
        .try_state::<crate::downloads::GestureState>()
        .map(|g| g.recent(&label, Duration::from_secs(10)))
        .unwrap_or(false);
    eprintln!("[e2e] security: direct invoke recorded={direct} invokeResult={invoke_result}");
    // The direct invoke arms the gate: re-arm right before the click so
    // the 3s gesture window is fresh, then the same download must land.
    let _ = eval_in(
        &wv,
        "window.__TAURI_INTERNALS__.invoke('note_user_gesture', {}); 'sent'",
    );
    std::thread::sleep(Duration::from_secs(1));
    let _ = eval_in(&wv, "document.getElementById('dl').click(); 'clicked'");
    std::thread::sleep(Duration::from_secs(4));
    let direct_allowed = file.exists();
    eprintln!("[e2e] security: direct-gesture download allowed={direct_allowed}");
    eprintln!("[e2e] security: flow done");
}

// Driven by `APPMAKA_DEBUG_MEDIA_MODE=media`. The shell script seeds the
// "sec" app; this proves the 0.18.11 media arms through the same invoke
// path the launcher tile toggle uses:
//   1. `media_play_pause` toggles the window's audio/paused state
//      (optimistic: the state flips from the pre-eval value, since the
//      test page has no real media element for the JS to control);
//   2. `media_next` / `media_prev` arms return ok;
//   3. after the second pause toggle the Now Playing bar renders in the
//      main window (it shows for playingAudio || mediaPaused).
// Logs `[e2e] media: ...` lines for the shell script to assert on.
// Same contract as the other flows: debug builds only, inert without the
// env var, detached thread, never blocks startup.
pub fn maybe_run_media_flow(app: &AppHandle) {
    if std::env::var("APPMAKA_DEBUG_MEDIA_MODE").unwrap_or_default() != "media" {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-media".to_string())
        .spawn(move || run_media_flow(&app));
}

fn media_cmd(main: &tauri::Webview, action: &str) -> String {
    let _ = eval_in(
        main,
        &format!(
            "window.__e2eMedia = 'pending'; window.__APPMAKA_INTERNALS__.invoke('account_window_command', {{appId: 'sec', accountId: 'sec-a', action: '{action}'}}).then(function(v){{ window.__e2eMedia = 'ok:' + v; }}).catch(function(e){{ window.__e2eMedia = 'err:' + e; }}); 'sent'"
        ),
    );
    std::thread::sleep(std::time::Duration::from_secs(2));
    eval_in(main, "window.__e2eMedia").unwrap_or_default()
}

fn media_row(main: &tauri::Webview) -> String {
    let _ = eval_in(
        main,
        "window.__e2eMediaList = 'pending'; window.__APPMAKA_INTERNALS__.invoke('list_open_account_windows', {}).then(function(ws){ var w = ws.find(function(x){ return x.label === 'acct-sec-sec-a'; }); window.__e2eMediaList = w ? ('playing:' + w.playingAudio + ' paused:' + w.mediaPaused) : 'missing'; }).catch(function(e){ window.__e2eMediaList = 'err:' + e; }); 'sent'",
    );
    std::thread::sleep(std::time::Duration::from_secs(2));
    eval_in(main, "window.__e2eMediaList").unwrap_or_default()
}

fn run_media_flow(app: &AppHandle) {
    use std::time::Duration;
    std::thread::sleep(Duration::from_secs(6));
    let (Some(store), Some(adblock), Some(winstate)) = (
        app.try_state::<crate::store::AppStore>(),
        app.try_state::<crate::adblock::AdblockState>(),
        app.try_state::<crate::windows::WindowState>(),
    ) else {
        eprintln!("[e2e] media: missing state");
        return;
    };
    if let Err(e) =
        crate::windows::open_account(app, &store, &adblock, &winstate, "sec", "sec-a", false)
    {
        eprintln!("[e2e] media: open failed: {e}");
        return;
    }
    let label = crate::windows::account_window_label("sec", "sec-a");
    std::thread::sleep(Duration::from_secs(8));
    if app.get_webview(&label).is_none() {
        eprintln!("[e2e] media: no webview for {label}");
        return;
    }
    let Some(main) = app.get_webview("main") else {
        eprintln!("[e2e] media: no-main-webview");
        return;
    };
    // 1. First pause toggle: optimistic state flips to playing.
    let first = media_cmd(&main, "media_play_pause");
    let row1 = media_row(&main);
    eprintln!("[e2e] media: toggle1={first} row1={row1}");
    // 2. Second toggle: playing -> paused, so the bar must appear.
    let second = media_cmd(&main, "media_play_pause");
    let row2 = media_row(&main);
    eprintln!("[e2e] media: toggle2={second} row2={row2}");
    // The bar refreshes on appmaka:media-changed (no more 2s poll); give
    // it a beat, then check the DOM.
    std::thread::sleep(Duration::from_secs(3));
    let bar = eval_in(
        &main,
        "document.querySelector('.now-playing-bar') !== null",
    )
    .unwrap_or_default();
    eprintln!("[e2e] media: bar_present={bar}");
    // 3. Next / prev arms return ok.
    let next = media_cmd(&main, "media_next");
    let prev = media_cmd(&main, "media_prev");
    eprintln!("[e2e] media: next={next} prev={prev}");
    // 4. Toggle back so the window leaves the test unpaused.
    let third = media_cmd(&main, "media_play_pause");
    let row3 = media_row(&main);
    eprintln!("[e2e] media: toggle3={third} row3={row3}");
    eprintln!("[e2e] media: flow done");
}

// Driven by `APPMAKA_DEBUG_LAUNCHER_MODE=launcher`. The 0.18.12
// stutter-fix proof, in one run:
//   1. Mount the Now Playing bar (open the seeded "sec" app, toggle to
//      paused) so the event-driven refresh is live, then idle 10s: the
//      debug counter for list_open_account_windows must not climb (the old
//      2s interval would add ~5 calls; one stray focus refresh is tolerated).
//   2. Summon the launcher through the real toggle path (hide, then
//      toggle_main_window) and confirm no "perf / Launcher summon timing"
//      entry lands in errors.json — routine hotkey use must not pollute
//      the error log (v0.18.14). Also logs summon_focus_ms (hide ->
//      toggle -> window reports focused).
//   3. Type in the launcher search input via eval and confirm the input
//      value + filtered tile count respond — this is the check that launcher
//      interaction still works with the main-window Focused(true) ->
//      focus_webview call restored (v0.18.14). If it fails, that
//      restoration must be re-examined, not worked around.
pub fn maybe_run_launcher_flow(app: &AppHandle) {
    if std::env::var("APPMAKA_DEBUG_LAUNCHER_MODE").unwrap_or_default() != "launcher" {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-launcher".to_string())
        .spawn(move || run_launcher_flow(&app));
}

#[cfg(debug_assertions)]
fn launcher_list_open_calls() -> u64 {
    crate::windows::DEBUG_LIST_OPEN_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(debug_assertions))]
fn launcher_list_open_calls() -> u64 {
    0
}

fn run_launcher_flow(app: &AppHandle) {
    use std::time::Duration;
    std::thread::sleep(Duration::from_secs(6));
    let Some(main) = app.get_webview("main") else {
        eprintln!("[e2e] launcher: no-main-webview");
        return;
    };
    // Mount the Now Playing bar: open the seeded sec app and toggle to
    // paused (same path as the media flow; the bar shows while paused).
    let (Some(store), Some(adblock), Some(winstate)) = (
        app.try_state::<crate::store::AppStore>(),
        app.try_state::<crate::adblock::AdblockState>(),
        app.try_state::<crate::windows::WindowState>(),
    ) else {
        eprintln!("[e2e] launcher: missing state");
        return;
    };
    if let Err(e) =
        crate::windows::open_account(app, &store, &adblock, &winstate, "sec", "sec-a", false)
    {
        eprintln!("[e2e] launcher: open failed: {e}");
        return;
    }
    std::thread::sleep(Duration::from_secs(8));
    let _ = media_cmd(&main, "media_play_pause");
    let _ = media_cmd(&main, "media_play_pause");
    let row = media_row(&main);
    eprintln!("[e2e] launcher: bar mount row={row}");
    let bar = eval_in(&main, "document.querySelector('.now-playing-bar') !== null")
        .unwrap_or_default();
    eprintln!("[e2e] launcher: bar_present={bar}");
    // Idle 10s: the event-driven bar must not poll.
    std::thread::sleep(Duration::from_secs(2));
    let before = launcher_list_open_calls();
    std::thread::sleep(Duration::from_secs(10));
    let after = launcher_list_open_calls();
    eprintln!(
        "[e2e] launcher: list_open_calls_before={before} after={after} delta={}",
        after.saturating_sub(before)
    );
    // Summon through the real toggle path and confirm no perf entry
    // pollutes errors.json (v0.18.14: the summon-timing record is gone).
    // Also time the summon: hide -> toggle -> window reports focused.
    // NOTE: the restored main-window Focused(true) -> focus_webview call
    // is Windows-only (#[cfg(windows)] MoveFocus), so the sandbox proves
    // the toggle path + the launcher page taking focus (input below),
    // not MoveFocus itself — that leg is PC-gated, like real clicks.
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
    }
    std::thread::sleep(Duration::from_secs(1));
    let summon_start = std::time::Instant::now();
    crate::toggle_main_window(app);
    let mut summon_focus_ms: u128 = 2000;
    for _ in 0..100 {
        let focused = app
            .get_webview_window("main")
            .map(|w| w.is_focused().unwrap_or(false))
            .unwrap_or(false);
        if focused {
            summon_focus_ms = summon_start.elapsed().as_millis();
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let doc_focused = eval_in(&main, "document.hasFocus()").unwrap_or_default();
    eprintln!("[e2e] launcher: summon_focus_ms={summon_focus_ms} doc_focused={doc_focused}");
    let perf = app
        .path()
        .app_data_dir()
        .map(|d| std::fs::read_to_string(d.join("errors.json")).unwrap_or_default())
        .unwrap_or_default()
        .contains("Launcher summon timing");
    eprintln!("[e2e] launcher: summon_perf_logged={perf} (want false)");
    // Type in the launcher search input and confirm it responds.
    let tiles_before =
        eval_in(&main, "document.querySelectorAll('.tile-label').length").unwrap_or_default();
    let typed = eval_in(
        &main,
        "(() => { const inp = document.querySelector('input[placeholder^=\"Search apps\"]'); if (!inp) return 'no-input'; inp.focus(); const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set; setter.call(inp, 'Example Two'); inp.dispatchEvent(new Event('input', {bubbles: true})); return 'focused=' + (document.activeElement === inp) + ' value=' + inp.value; })()",
    )
    .unwrap_or_default();
    std::thread::sleep(Duration::from_secs(1));
    let tiles_after = eval_in(
        &main,
        "document.querySelectorAll('.tile-label').length + '|' + Array.from(document.querySelectorAll('.tile-label')).map(e => e.textContent).join(',')",
    )
    .unwrap_or_default();
    eprintln!("[e2e] launcher: tiles_before={tiles_before} typed={typed} tiles_after={tiles_after}");
    eprintln!("[e2e] launcher: flow done");
}

// ---------------------------------------------------------------------------
// Theme screenshot driver (v0.18.14 Area 2).
//
// Driven by `APPMAKA_DEBUG_THEME_SHOT=1`. Walks the views the QA
// screenshots need — launcher, the Now Playing bar (mounted via the sec
// media toggle, same as the launcher flow), library, one app's Settings
// form — logging `theme-shot: ready <view>` at each stop and waiting
// there so the shell script can screenshot. Also logs the resolved
// theme and the body's computed colors, so a wrong-theme run is visible
// in the log, not just in pixels.
//
// Same contract as the other flows: debug builds only, inert without
// the env var, detached thread, never blocks startup.
// ---------------------------------------------------------------------------

/// Called once from setup. Returns immediately.
pub fn maybe_run_theme_shot_flow(app: &AppHandle) {
    if std::env::var("APPMAKA_DEBUG_THEME_SHOT").unwrap_or_default() != "1" {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-theme-shot".to_string())
        .spawn(move || run_theme_shot_flow(&app));
}

fn run_theme_shot_flow(app: &AppHandle) {
    use std::time::Duration;
    std::thread::sleep(Duration::from_secs(6));
    let Some(main) = app.get_webview("main") else {
        eprintln!("[theme-shot] no-main-webview");
        return;
    };
    let theme = eval_in(
        &main,
        "document.documentElement.dataset.theme + '|' + getComputedStyle(document.body).backgroundColor + '|' + getComputedStyle(document.body).color",
    )
    .unwrap_or_default();
    eprintln!("[theme-shot] theme={theme}");
    eprintln!("[theme-shot] ready launcher");
    std::thread::sleep(Duration::from_secs(8));
    // Mount the Now Playing bar: open the seeded sec app and toggle to
    // paused (same path as the launcher flow).
    let (Some(store), Some(adblock), Some(winstate)) = (
        app.try_state::<crate::store::AppStore>(),
        app.try_state::<crate::adblock::AdblockState>(),
        app.try_state::<crate::windows::WindowState>(),
    ) else {
        eprintln!("[theme-shot] missing state");
        return;
    };
    if let Err(e) =
        crate::windows::open_account(app, &store, &adblock, &winstate, "sec", "sec-a", false)
    {
        eprintln!("[theme-shot] open failed: {e}");
        return;
    }
    std::thread::sleep(Duration::from_secs(8));
    let _ = media_cmd(&main, "media_play_pause");
    let _ = media_cmd(&main, "media_play_pause");
    let bar = eval_in(&main, "document.querySelector('.now-playing-bar') !== null")
        .unwrap_or_default();
    eprintln!("[theme-shot] bar_present={bar}");
    eprintln!("[theme-shot] ready now-playing");
    std::thread::sleep(Duration::from_secs(8));
    // Library: the launcher footer's "Manage apps" button.
    let lib = eval_in(
        &main,
        "(() => { const b = Array.from(document.querySelectorAll('button')).find(x => x.textContent.trim() === 'Manage apps'); if (!b) return 'no-button'; b.click(); return 'clicked'; })()",
    )
    .unwrap_or_default();
    std::thread::sleep(Duration::from_secs(2));
    eprintln!("[theme-shot] library={lib}");
    eprintln!("[theme-shot] ready library");
    std::thread::sleep(Duration::from_secs(8));
    // Settings: the first app row's Settings toggle. The panel expands
    // inside that app's card, below the fold behind the Now Playing bar,
    // so scroll the form's "Settings" heading into view before the shot.
    let set = eval_in(
        &main,
        "(() => { const b = document.querySelector('.settings-toggle'); if (!b) return 'no-button'; b.click(); return 'clicked'; })()",
    )
    .unwrap_or_default();
    std::thread::sleep(Duration::from_secs(2));
    let set_scroll = eval_in(
        &main,
        "(() => { const h = Array.from(document.querySelectorAll('h3')).find(x => x.textContent.trim() === 'Settings'); if (!h) return 'no-form'; h.scrollIntoView({ block: 'center' }); return 'scrolled'; })()",
    )
    .unwrap_or_default();
    eprintln!("[theme-shot] settings={set}/{set_scroll}");
    eprintln!("[theme-shot] ready settings");
    std::thread::sleep(Duration::from_secs(8));
    eprintln!("[theme-shot] flow done");
}

// ---------------------------------------------------------------------------
// Websearch "add this site" E2E driver (Xvfb smoke).
//
// Driven by `APPMAKA_DEBUG_WEBSEARCH_MODE=add` (+ optional
// `APPMAKA_DEBUG_WEBSEARCH_QUERY`, default "example domain", and
// `APPMAKA_DEBUG_WEBSEARCH_TARGET`, default "https://example.com").
// Opens a real web search, proves the results-page guard refuses the add
// while the window is still showing results, navigates the window to the
// target (standing in for clicking a result — WebKitGTK webviews under
// Xvfb don't take scripted clicks), focuses it, and waits for the shell
// script to press Ctrl+Shift+A via xdotool; then polls the library until
// the target appears as an app. Logs `[e2e] websearch:` lines for the
// shell script to assert on.
//
// Same contract as the other flows: debug builds only, inert without the
// env var, detached thread, never blocks startup.
// ---------------------------------------------------------------------------

/// Called once from setup. Returns immediately.
pub fn maybe_run_websearch_add_flow(app: &AppHandle) {
    if std::env::var("APPMAKA_DEBUG_WEBSEARCH_MODE").unwrap_or_default() != "add" {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-websearch".to_string())
        .spawn(move || run_websearch_add_flow(&app));
}

fn run_websearch_add_flow(app: &AppHandle) {
    use std::time::Duration;
    let query = std::env::var("APPMAKA_DEBUG_WEBSEARCH_QUERY")
        .unwrap_or_else(|_| "example domain".to_string());
    let target = std::env::var("APPMAKA_DEBUG_WEBSEARCH_TARGET")
        .unwrap_or_else(|_| "https://example.com".to_string());
    std::thread::sleep(Duration::from_secs(5));
    let (Some(store), Some(adblock), Some(winstate)) = (
        app.try_state::<crate::store::AppStore>(),
        app.try_state::<crate::adblock::AdblockState>(),
        app.try_state::<crate::windows::WindowState>(),
    ) else {
        eprintln!("[e2e] websearch: missing state");
        return;
    };
    if let Err(e) = crate::websearch::open_search_window(app, &adblock, &query) {
        eprintln!("[e2e] websearch: open failed: {e}");
        return;
    }
    let mut window = None;
    for _ in 0..20 {
        if let Some(w) = app.get_webview_window(crate::websearch::SEARCH_WINDOW_LABEL) {
            window = Some(w);
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let Some(window) = window else {
        eprintln!("[e2e] websearch: no window");
        return;
    };
    let mut results_url = String::new();
    for _ in 0..30 {
        if let Ok(u) = window.url() {
            let s = u.to_string();
            if s.contains("q=") {
                results_url = s;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if results_url.is_empty() {
        eprintln!("[e2e] websearch: results never committed");
        return;
    }
    eprintln!("[e2e] websearch: results url {results_url}");
    // Guard: adding the results page itself must be refused.
    match crate::popup_add::run_add(
        app,
        &store,
        &adblock,
        &winstate,
        crate::websearch::SEARCH_WINDOW_LABEL,
    ) {
        Err(e) if e == crate::popup_add::MSG_SEARCH_RESULTS => {
            eprintln!("[e2e] websearch: guard ok");
        }
        Err(e) => {
            eprintln!("[e2e] websearch: guard FAIL wrong error: {e}");
            return;
        }
        Ok(_) => {
            eprintln!("[e2e] websearch: guard FAIL results page was added");
            return;
        }
    }
    // Stand in for clicking a result: navigate to the target.
    // Compare hosts, not full strings: url::Url normalizes
    // "https://example.com" to "https://example.com/" on commit, so an
    // exact-string match could never fire even after a good landing.
    let target_host = url::Url::parse(&target)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();
    let js = format!(
        "window.location.href={};",
        serde_json::to_string(&target).unwrap_or_default()
    );
    if let Err(e) = window.eval(&js) {
        eprintln!("[e2e] websearch: navigate failed: {e}");
        return;
    }
    let mut landed = false;
    for _ in 0..30 {
        if let Ok(u) = window.url() {
            if u.host_str().map(|h| h.to_string()) == Some(target_host.clone()) {
                landed = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if !landed {
        eprintln!("[e2e] websearch: target never landed");
        return;
    }
    eprintln!("[e2e] websearch: landed {target}");
    // Focus the window so the hotkey dispatch finds it, then wait for the
    // shell script to press Ctrl+Shift+A for real (xdotool).
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
    std::thread::sleep(Duration::from_secs(1));
    eprintln!("[e2e] websearch: ready-for-hotkey");
    // Poll the library until the target appears as an app.
    let mut added = false;
    for _ in 0..30 {
        let found = store
            .list()
            .map(|apps| {
                apps.iter().any(|a| {
                    url::Url::parse(&a.url)
                        .ok()
                        .and_then(|u| u.host_str().map(|h| h.to_string()))
                        == Some(target_host.clone())
                })
            })
            .unwrap_or(false);
        if found {
            added = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if added {
        eprintln!("[e2e] websearch: added {target_host}");
    } else {
        eprintln!("[e2e] websearch: NOT added (timeout)");
    }
    eprintln!("[e2e] websearch: flow done");
}

// Driven by `APPMAKA_DEBUG_POPUP_PERMS_MODE=panel`.
// Proves the Popup permissions panel in the main window: reads each row's
// policy select + allowlist + accounts line straight from the DOM, flips
// one app's policy through the panel's own controls (native select setter
// + change event, then the row's Save button click — the same
// update_app_settings command the panel uses), and confirms the stored
// setting via the AppStore. Also reports whether the global-override
// banner shows and the rows are disabled, so the same flow covers a
// global-on run. WebKitGTK webviews under Xvfb don't take scripted mouse
// clicks, so the flip goes through DOM evaluation instead.
//
// Same contract as the other flows: debug builds only, inert without the
// env var, detached thread, never blocks startup.
// ---------------------------------------------------------------------------

/// Called once from setup. Returns immediately.
pub fn maybe_run_popup_permissions_flow(app: &AppHandle) {
    let mode = std::env::var("APPMAKA_DEBUG_POPUP_PERMS_MODE").unwrap_or_default();
    if mode != "panel" && mode != "panel-collapse" {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-popup-perms".to_string())
        .spawn(move || run_popup_permissions_flow(&app, &mode));
}

/// Collapse toggle, driven through the panel's own controls: collapse
/// all, expand one row by clicking its summary, collapse all again,
/// expand all. Each step asserts the row count in each form (expanded
/// rows carry a select, collapsed rows a one-line summary button).
/// Ends expanded unless `leave_collapsed` is set, so the caller picks
/// the screenshot state. Returns "ok" or a "FAIL: ..." reason.
fn collapse_phase(main: &tauri::Webview, leave_collapsed: bool) -> String {
    use std::time::Duration;
    let read = |js: &str| unquote(eval_in(main, js).unwrap_or_default());
    let counts = || {
        read(
            r#"(() => {
          const sels = document.querySelectorAll('.popup-permissions-row select').length;
          const sums = Array.from(document.querySelectorAll('.popup-permissions-summary'))
            .map(b => Array.from(b.querySelectorAll('span')).map(s => s.textContent).join(' '));
          return 'selects=' + sels + ' summaries=[' + sums.join(' | ') + ']';
        })()"#,
        )
    };
    let label = || {
        read("document.querySelector('.popup-permissions-toggle')?.textContent ?? 'none'")
    };
    let click_toggle = || {
        read(
            r#"(() => { document.querySelector('.popup-permissions-toggle')?.click(); return 'clicked'; })()"#,
        )
    };
    let collapsed_all =
        "selects=0 summaries=[Example One example.com Block | Example Two example.org Block | Example Three example.net Allow]";
    let expanded_all = "selects=3 summaries=[]";
    let one_expanded =
        "selects=1 summaries=[Example One example.com Block | Example Three example.net Allow]";

    if label() != "Collapse all" {
        return format!("FAIL: initial toggle label={}", label());
    }
    click_toggle();
    std::thread::sleep(Duration::from_millis(500));
    if counts() != collapsed_all {
        return format!("FAIL: after collapse-all got {}", counts());
    }
    if label() != "Show all" {
        return format!("FAIL: toggle label after collapse-all={}", label());
    }
    let clicked = read(
        r#"(() => { const b = Array.from(document.querySelectorAll('.popup-permissions-summary')).find(b => b.textContent.includes('Example Two')); if (!b) return 'no-summary'; b.click(); return 'clicked'; })()"#,
    );
    if clicked != "clicked" {
        return format!("FAIL: expand-one summary click={clicked}");
    }
    std::thread::sleep(Duration::from_millis(500));
    if counts() != one_expanded {
        return format!("FAIL: after expand-one got {}", counts());
    }
    if label() != "Collapse all" {
        return format!("FAIL: toggle label after expand-one={}", label());
    }
    click_toggle();
    std::thread::sleep(Duration::from_millis(500));
    if counts() != collapsed_all {
        return format!("FAIL: after second collapse-all got {}", counts());
    }
    click_toggle();
    std::thread::sleep(Duration::from_millis(500));
    if counts() != expanded_all {
        return format!("FAIL: after expand-all got {}", counts());
    }
    if label() != "Collapse all" {
        return format!("FAIL: toggle label after expand-all={}", label());
    }
    if leave_collapsed {
        click_toggle();
        std::thread::sleep(Duration::from_millis(500));
        if counts() != collapsed_all {
            return format!("FAIL: final collapse got {}", counts());
        }
    }
    "ok".to_string()
}

/// eval_in returns JS strings JSON-quoted ("flipped"); strip the quotes
/// so log lines and comparisons see the bare value. Numbers come back
/// unquoted, so this is a no-op for them.
fn unquote(s: String) -> String {
    s.trim_matches('"').to_string()
}

fn run_popup_permissions_flow(app: &AppHandle, mode: &str) {
    use std::time::Duration;
    std::thread::sleep(Duration::from_secs(6));
    let Some(main) = app.get_webview("main") else {
        eprintln!("[e2e] popup-perms: no-main-webview");
        return;
    };
    let Some(store) = app.try_state::<crate::store::AppStore>() else {
        eprintln!("[e2e] popup-perms: missing store");
        return;
    };
    // The panel lives in the library view (the default is the launcher
    // tiles); switch to it — this also shows and focuses the main window.
    if let Err(e) = crate::show_library_view(app) {
        eprintln!("[e2e] popup-perms: show-library failed: {e}");
        return;
    }
    // Wait for the panel rows to mount (React renders after list_apps).
    let mut mounted = false;
    for _ in 0..20 {
        let n = eval_in(
            &main,
            "document.querySelectorAll('.popup-permissions-row').length",
        )
        .unwrap_or_default();
        if !n.is_empty() && n != "0" {
            mounted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if !mounted {
        eprintln!("[e2e] popup-perms: rows never mounted");
        return;
    }
    // Collapse toggle first: it needs no backend round-trip, and the
    // flip below needs the rows expanded. The collapse-only mode stops
    // here with the panel collapsed, for its screenshot.
    let collapse = collapse_phase(&main, mode == "panel-collapse");
    eprintln!("[e2e] popup-perms: collapse={collapse}");
    if collapse != "ok" {
        return;
    }
    if mode == "panel-collapse" {
        let _ = main.eval(
            "document.querySelector('.popup-permissions-list')?.closest('section')?.scrollIntoView({block: 'start'});",
        );
        std::thread::sleep(Duration::from_secs(1));
        eprintln!("[e2e] popup-perms: flow done");
        return;
    }
    let state = unquote(eval_in(
        &main,
        r#"(() => {
          const rows = Array.from(document.querySelectorAll('.popup-permissions-row')).map(r => {
            const name = r.querySelector('h3')?.textContent ?? '';
            const policy = r.querySelector('select')?.value ?? '';
            const allow = (r.querySelector('textarea')?.value ?? '').split('\n').filter(Boolean).join(',');
            const acc = Array.from(r.querySelectorAll('p.muted.small')).map(p => p.textContent).find(t => t.startsWith('Accounts:')) ?? '';
            return name + '=' + policy + '|' + allow + '|' + acc;
          });
          const banner = document.body.innerText.includes('Popups are allowed in all apps right now');
          const disabled = document.querySelector('.popup-permissions-row select')?.disabled ?? false;
          return 'rows=' + rows.length + ' state=[' + rows.join(';') + '] banner=' + banner + ' disabled=' + disabled;
        })()"#,
    )
    .unwrap_or_default());
    eprintln!("[e2e] popup-perms: {state}");
    // When the global override is on the rows are disabled: there is
    // nothing to flip. Still prove the Save button refuses the click below.
    let rows_disabled = state.contains("disabled=true");
    let flipped = if rows_disabled {
        "skipped-disabled".to_string()
    } else {
        // Flip Example Two from block to allow through the panel's own controls.
        unquote(eval_in(
            &main,
            r#"(() => {
          const row = Array.from(document.querySelectorAll('.popup-permissions-row')).find(r => r.querySelector('h3')?.textContent === 'Example Two');
          if (!row) return 'no-row';
          const sel = row.querySelector('select');
          if (!sel) return 'no-select';
          const setter = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
          setter.call(sel, 'allow');
          sel.dispatchEvent(new Event('change', {bubbles: true}));
          return 'flipped';
        })()"#,
        )
        .unwrap_or_default())
    };
    eprintln!("[e2e] popup-perms: flip={flipped}");
    if !rows_disabled && flipped != "flipped" {
        return;
    }
    // Let React re-render so the Save button enables, then click it.
    std::thread::sleep(Duration::from_secs(1));
    let clicked = unquote(eval_in(
        &main,
        r#"(() => {
          const row = Array.from(document.querySelectorAll('.popup-permissions-row')).find(r => r.querySelector('h3')?.textContent === 'Example Two');
          const btn = row?.querySelector('button[type="submit"]');
          if (!btn) return 'no-btn';
          if (btn.disabled) return 'btn-disabled';
          btn.click();
          return 'clicked';
        })()"#,
    )
    .unwrap_or_default());
    eprintln!("[e2e] popup-perms: save={clicked}");
    if clicked == "clicked" {
        // The panel's save is one invoke round-trip; poll the store until the
        // new policy lands, then report what is actually stored.
        let mut stored = String::new();
        for _ in 0..20 {
            if let Ok(a) = store.get("ex2") {
                if a.settings.popup_policy == "allow" {
                    stored = format!(
                        "policy={} allowlist=[{}]",
                        a.settings.popup_policy,
                        a.settings.popup_allowlist.join(",")
                    );
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if stored.is_empty() {
            eprintln!("[e2e] popup-perms: stored FAIL ex2 never became allow");
            return;
        }
        eprintln!("[e2e] popup-perms: stored ex2 {stored}");
    }
    // A disabled Save (global override on) is not a failure: the run's
    // purpose there is the banner + disabled rows, already logged above.
    // Scroll the whole panel section (heading + banner + rows) into view
    // for the screenshot, then finish.
    let _ = main.eval(
        "document.querySelector('.popup-permissions-list')?.closest('section')?.scrollIntoView({block: 'start'});",
    );
    std::thread::sleep(Duration::from_secs(1));
    eprintln!("[e2e] popup-perms: flow done");
}

// ---------------------------------------------------------------------------
// 0.18.14: auto-allow site permissions toggle + dictation "small" model.
// Driven by `APPMAKA_DEBUG_01814_MODE=toggle` or `=model`. Same contract
// as the other flows: debug builds only, inert without the env var,
// detached thread, never blocks startup.
//
// The flow proves, in one run:
//   - the permission decision reads the LIVE setting: with the toggle
//     off every kind answers Default; with it on, microphone, camera,
//     geolocation, and notifications answer Allow while file access,
//     automatic downloads, and window management still answer Default;
//     flipping it back off restores Default (no stale capture).
//   - the Settings checkbox itself flips the stored setting end to
//     end (click -> invoke -> saved setting), and the decision follows
//     the click immediately.
//   - the dictation backend accepts "small" (previously rejected as
//     "Unknown speech model") and the Speech model group offers three
//     radios, with small present.
// Mode "toggle" ends with the Launcher settings toggle scrolled into
// view; mode "model" ends with the Clipboard speech-model radios in
// view, for the two screenshots.
// ---------------------------------------------------------------------------

/// Called once from setup. Returns immediately.
pub fn maybe_run_01814_flow(app: &AppHandle) {
    let mode = std::env::var("APPMAKA_DEBUG_01814_MODE").unwrap_or_default();
    if mode != "toggle" && mode != "model" {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-01814".to_string())
        .spawn(move || run_01814_flow(&app, &mode));
}

fn run_01814_flow(app: &AppHandle, mode: &str) {
    use std::time::Duration;
    use tauri::webview::{PermissionKind, PermissionResponse};
    std::thread::sleep(Duration::from_secs(6));
    let Some(main) = app.get_webview("main") else {
        eprintln!("[e2e] 01814: no-main-webview");
        return;
    };
    // Both new controls live in the library view: the toggle in the
    // Launcher settings panel, the model radios in the Clipboard panel.
    if let Err(e) = crate::show_library_view(app) {
        eprintln!("[e2e] 01814: show-library failed: {e}");
        return;
    }
    // The Launcher section is collapsed by default (`launcherPanelOpen`
    // starts false), so the toggle is not in the DOM until the section's
    // "Show" button is clicked. Open it first — poll, because the
    // library view renders after the settings load.
    let mut opened = String::new();
    for _ in 0..20 {
        opened = unquote(eval_in(
            &main,
            r#"(() => {
              const btns = Array.from(document.querySelectorAll('button.text-button[aria-expanded]'));
              const btn = btns.find(b => b.closest('section')?.querySelector('h2')?.textContent === 'Launcher');
              if (!btn) return 'no-launcher-btn';
              if (btn.getAttribute('aria-expanded') === 'true') return 'already-open';
              btn.click();
              return 'opened';
            })()"#,
        )
        .unwrap_or_default());
        if opened == "opened" || opened == "already-open" {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if opened != "opened" && opened != "already-open" {
        eprintln!("[e2e] 01814: FAIL launcher section open={opened}");
        return;
    }
    eprintln!("[e2e] 01814: launcher-section={opened}");
    std::thread::sleep(Duration::from_millis(500));
    // Wait for the toggle to mount (React renders after settings load).
    let mut mounted = false;
    for _ in 0..20 {
        let n = eval_in(
            &main,
            "document.querySelectorAll('input[aria-label=\"Automatically allow site permissions\"]').length",
        )
        .unwrap_or_default();
        if !n.is_empty() && n != "0" {
            mounted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if !mounted {
        eprintln!("[e2e] 01814: toggle never mounted");
        return;
    }
    // The live decision, in one line per run: mic/cam/geo/notif plus
    // the three kinds that must never auto-allow.
    let decide = |main: &tauri::Webview| -> String {
        [
            ("mic", PermissionKind::Microphone),
            ("cam", PermissionKind::Camera),
            ("geo", PermissionKind::Geolocation),
            ("notif", PermissionKind::Notifications),
            ("fs", PermissionKind::FileSystemAccess),
            ("dl", PermissionKind::AutomaticDownloads),
            ("wm", PermissionKind::WindowManagement),
        ]
        .iter()
        .map(|(name, kind)| {
            let r = crate::launcher_settings::decide_permission(main, *kind);
            format!(
                "{name}={}",
                if r == PermissionResponse::Allow {
                    "allow"
                } else {
                    "default"
                }
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
    };
    let expect = |label: &str, got: &str, want: &str| -> bool {
        if got != want {
            eprintln!("[e2e] 01814: FAIL {label}: got {got} want {want}");
            return false;
        }
        eprintln!("[e2e] 01814: {label} {got}");
        true
    };
    let all_default =
        "mic=default cam=default geo=default notif=default fs=default dl=default wm=default";
    let on = "mic=allow cam=allow geo=allow notif=allow fs=default dl=default wm=default";
    // Off: everything answers the platform default.
    if !expect("decide-off", &decide(&main), all_default) {
        return;
    }
    // On, via the command: the four kinds allow, the rest stay default.
    if let Err(e) = crate::launcher_settings::set_auto_allow_permissions(app.clone(), true) {
        eprintln!("[e2e] 01814: FAIL set-on: {e}");
        return;
    }
    if !expect("decide-on", &decide(&main), on) {
        return;
    }
    // Back off, via the command: the decision follows immediately.
    if let Err(e) = crate::launcher_settings::set_auto_allow_permissions(app.clone(), false) {
        eprintln!("[e2e] 01814: FAIL set-off: {e}");
        return;
    }
    if !expect("decide-off-again", &decide(&main), all_default) {
        return;
    }
    // The checkbox itself, end to end: click -> invoke -> saved setting.
    let clicked = unquote(eval_in(
        &main,
        r#"(() => { const c = document.querySelector('input[aria-label="Automatically allow site permissions"]'); if (!c) return 'no-checkbox'; c.click(); return 'clicked'; })()"#,
    )
    .unwrap_or_default());
    if clicked != "clicked" {
        eprintln!("[e2e] 01814: FAIL toggle click={clicked}");
        return;
    }
    std::thread::sleep(Duration::from_secs(1));
    let checked = unquote(eval_in(
        &main,
        "document.querySelector('input[aria-label=\"Automatically allow site permissions\"]')?.checked",
    )
    .unwrap_or_default());
    eprintln!("[e2e] 01814: toggle=clicked checked={checked}");
    if checked != "true" {
        eprintln!("[e2e] 01814: FAIL toggle checked={checked}");
        return;
    }
    // The UI-driven change is live for the decision at once.
    if !expect("decide-after-ui-click", &decide(&main), on) {
        return;
    }
    // Dictation backend: "small" was rejected before this release.
    match crate::dictation::set_dictation_model(app.clone(), "small".to_string()) {
        Ok(_) => eprintln!("[e2e] 01814: model-small=ok"),
        Err(e) => {
            eprintln!("[e2e] 01814: FAIL model-small: {e}");
            return;
        }
    }
    // Restore the default so the seeded run leaves nothing behind.
    match crate::dictation::set_dictation_model(app.clone(), "tiny".to_string()) {
        Ok(_) => eprintln!("[e2e] 01814: model-restore=ok"),
        Err(e) => {
            eprintln!("[e2e] 01814: FAIL model-restore: {e}");
            return;
        }
    }
    // Dictation UI: three radios, with small present.
    let mut radios = String::new();
    for _ in 0..20 {
        radios = unquote(eval_in(
            &main,
            r#"(() => {
              const rs = Array.from(document.querySelectorAll('input[name="dictation-model"]'));
              const labels = rs.map(r => r.closest('label')?.textContent ?? '');
              return 'radios=' + rs.length + ' small=' + labels.some(t => t.includes('Small')) + ' checked=' + (rs.find(r => r.checked)?.closest('label')?.textContent ?? 'none');
            })()"#,
        )
        .unwrap_or_default());
        if radios.starts_with("radios=3") {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    eprintln!("[e2e] 01814: {radios}");
    if !radios.starts_with("radios=3") || !radios.contains("small=true") {
        eprintln!("[e2e] 01814: FAIL radios");
        return;
    }
    // Scroll the requested control into view for the screenshot.
    if mode == "model" {
        let _ = main.eval(
            "document.querySelector('div[role=\"radiogroup\"][aria-label=\"Speech model\"]')?.scrollIntoView({block: 'center'});",
        );
    } else {
        let _ = main.eval(
            "document.querySelector('input[aria-label=\"Automatically allow site permissions\"]')?.scrollIntoView({block: 'center'});",
        );
    }
    std::thread::sleep(Duration::from_secs(1));
    eprintln!("[e2e] 01814: flow done");
}

// ---------------------------------------------------------------------------
// Quit flow (Windows debug CI).
//
// Driven by `APPMAKA_DEBUG_QUIT_AFTER_SECS=N`. After N seconds, performs
// the exact tray-Quit sequence (pin::set_shutting_down,
// session::write_session, session::clear_restore_sentinel, app.exit(0))
// so CI can measure clean-quit time from outside: PowerShell waits for
// the process to exit and fails if it is still alive after the timeout.
// Logs `[e2e] quit: requested` immediately before exiting.
//
// Same contract as the other flows: debug builds only, inert without the
// env var, detached thread, never blocks startup.
// ---------------------------------------------------------------------------

pub fn maybe_run_quit_flow(app: &AppHandle) {
    let secs: u64 = std::env::var("APPMAKA_DEBUG_QUIT_AFTER_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if secs == 0 {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-debug-quit".to_string())
        .spawn(move || run_quit_flow(&app, secs));
}

fn run_quit_flow(app: &AppHandle, secs: u64) {
    std::thread::sleep(std::time::Duration::from_secs(secs));
    eprintln!("[e2e] quit: requested after={secs}s");
    crate::pin::set_shutting_down();
    crate::session::write_session(app);
    crate::session::clear_restore_sentinel(app);
    app.exit(0);
}
