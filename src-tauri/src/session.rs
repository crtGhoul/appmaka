//! Session restore (v0.9.5): persist the open-window set — account windows
//! and the search window, each with its geometry — so the user's tabs come
//! back on the next launch.
//!
//! `session.json` lives in the app-data dir and is rewritten on every
//! window open/close, on debounced move/resize, and at app exit. Geometry
//! is stored in logical pixels (physical ÷ scale factor) and is never
//! saved while a window is minimized — minimized windows report junk
//! coordinates, so the last good position is kept instead. A corrupt or
//! missing file simply means "nothing to restore".

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::adblock::AdblockState;
use crate::launcher_settings::{LauncherSettings, StartupMode};
use crate::store::AppStore;
use crate::websearch::SEARCH_WINDOW_LABEL;
use crate::windows::{self, WindowPlacement, WindowState};

/// Logical-pixel rect of one saved window. None on an entry means "no
/// known position" (e.g. the window was minimized at every save) and
/// restores with the default centered placement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// One saved window, in open order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SessionWindow {
    Account {
        #[serde(rename = "appId", default)]
        app_id: String,
        #[serde(rename = "accountId", default)]
        account_id: String,
        #[serde(default)]
        rect: Option<WindowRect>,
        /// v0.9.9: per-window "Don't close this window". Old files lack it
        /// and load as unpinned — no migration needed.
        #[serde(default)]
        pinned: bool,
    },
    Search {
        #[serde(default)]
        query: String,
        #[serde(default)]
        rect: Option<WindowRect>,
        #[serde(default)]
        pinned: bool,
    },
    /// v0.10.0: tabbed app window. `id` is the stable group id (pins key
    /// off `tabbed:{id}`); old files lack the variant entirely.
    Tabbed {
        #[serde(default)]
        id: String,
        #[serde(default)]
        tabs: Vec<crate::tabs::TabEntry>,
        #[serde(default)]
        active: usize,
        #[serde(default)]
        rect: Option<WindowRect>,
        #[serde(default)]
        pinned: bool,
    },
}

impl SessionWindow {
    /// Whether the user switched on "Don't close this window" for this entry.
    pub fn pinned(&self) -> bool {
        match self {
            SessionWindow::Account { pinned, .. } => *pinned,
            SessionWindow::Search { pinned, .. } => *pinned,
            SessionWindow::Tabbed { pinned, .. } => *pinned,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub windows: Vec<SessionWindow>,
}

/// Logical-pixel rect of a monitor, for off-screen validation at restore.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalMonitor {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Millisecond clock for open-order timestamps (see windows.rs).
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse session JSON tolerantly: a corrupt file — or a single corrupt
/// entry — never takes down the whole restore; bad entries are skipped
/// and empty ids/queries are dropped.
pub fn parse_session(raw: &str) -> Session {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        windows: Vec<serde_json::Value>,
    }
    let raw: Raw =
        serde_json::from_str(raw).unwrap_or(Raw { windows: Vec::new() });
    let windows = raw
        .windows
        .into_iter()
        .filter_map(|v| serde_json::from_value::<SessionWindow>(v).ok())
        .filter(|w| match w {
            SessionWindow::Account {
                app_id,
                account_id,
                ..
            } => !app_id.is_empty() && !account_id.is_empty(),
            SessionWindow::Search { query, .. } => !query.trim().is_empty(),
            SessionWindow::Tabbed { id, tabs, .. } => {
                !id.is_empty() && tabs.iter().any(|t| t.valid())
            }
        })
        .collect();
    Session { windows }
}

/// Physical pixels → logical rect. None on a bogus scale factor.
pub fn physical_to_logical_rect(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale: f64,
) -> Option<WindowRect> {
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    Some(WindowRect {
        x: (x as f64 / scale).round() as i32,
        y: (y as f64 / scale).round() as i32,
        width: (width as f64 / scale).round().max(1.0) as u32,
        height: (height as f64 / scale).round().max(1.0) as u32,
    })
}

/// True when any part of the rect is visible on any monitor. An empty
/// monitor list can't validate — trust the rect (fail open; the window
/// manager places it sanely).
pub fn rect_visible_on_any(rect: &WindowRect, monitors: &[LogicalMonitor]) -> bool {
    if monitors.is_empty() {
        return true;
    }
    monitors.iter().any(|m| {
        let rx2 = rect.x as i64 + rect.width as i64;
        let ry2 = rect.y as i64 + rect.height as i64;
        let mx2 = m.x as i64 + m.width as i64;
        let my2 = m.y as i64 + m.height as i64;
        (rect.x as i64) < mx2
            && rx2 > m.x as i64
            && (rect.y as i64) < my2
            && ry2 > m.y as i64
    })
}

/// Placement for restore: the saved rect when it lands on a live monitor,
/// otherwise None — the window opens centered instead of off-screen.
pub fn placement_for_rect(
    rect: Option<&WindowRect>,
    monitors: &[LogicalMonitor],
) -> Option<WindowPlacement> {
    let r = rect?;
    if !rect_visible_on_any(r, monitors) {
        return None;
    }
    Some(WindowPlacement {
        x: r.x as f64,
        y: r.y as f64,
        width: r.width as f64,
        height: r.height as f64,
    })
}

/// Merge live geometry with the last saved position: a minimized (or
/// otherwise unreadable) window keeps its last good rect instead of junk.
pub fn merge_rect(
    live: Option<WindowRect>,
    prev: Option<WindowRect>,
) -> Option<WindowRect> {
    live.or(prev)
}

/// Pure session assembly, sorted by open time. The live-window plumbing
/// stays in `write_session`; this is the unit-testable core. `pinned_for`
/// resolves a window label to its pin state (reads the runtime PinState
/// map in production); labels are derivable without opening windows.
pub fn build_session(
    accounts: Vec<(String, String, Option<WindowRect>, u64)>,
    search: Option<(String, Option<WindowRect>, u64)>,
    tabbed: Vec<crate::tabs::TabbedRestoreSpec>,
    pinned_for: &dyn Fn(&str) -> bool,
) -> Session {
    let mut entries: Vec<(u64, SessionWindow)> = Vec::new();
    for (app_id, account_id, rect, opened_at) in accounts {
        let label = crate::windows::account_window_label(&app_id, &account_id);
        entries.push((
            opened_at,
            SessionWindow::Account {
                app_id,
                account_id,
                rect,
                pinned: pinned_for(&label),
            },
        ));
    }
    if let Some((query, rect, opened_at)) = search {
        entries.push((
            opened_at,
            SessionWindow::Search {
                query,
                rect,
                pinned: pinned_for(SEARCH_WINDOW_LABEL),
            },
        ));
    }
    // v0.10.0: tabbed windows. Pins key off the stable group id
    // (`tabbed:{id}`), never the generation-suffixed window label.
    for (id, tabs, active, rect, opened_at) in tabbed {
        let pinned = pinned_for(&crate::tabs::pin_key(&id));
        entries.push((
            opened_at,
            SessionWindow::Tabbed {
                id,
                tabs,
                active,
                rect,
                pinned,
            },
        ));
    }
    entries.sort_by_key(|(ts, _)| *ts);
    Session {
        windows: entries.into_iter().map(|(_, w)| w).collect(),
    }
}

/// Keep only restorable entries: unknown accounts are skipped silently.
/// Pure over a caller-supplied predicate so the store lookup stays at the
/// edge and the rule is unit-testable.
pub fn partition_restorable<'a>(
    session: &'a Session,
    account_exists: &dyn Fn(&str, &str) -> bool,
) -> Vec<&'a SessionWindow> {
    session
        .windows
        .iter()
        .filter(|w| match w {
            SessionWindow::Account {
                app_id,
                account_id,
                ..
            } => account_exists(app_id, account_id),
            SessionWindow::Search { .. } => true,
            // A tabbed window restores when at least one tab still
            // exists; dead tabs are dropped at open time.
            SessionWindow::Tabbed { tabs, .. } => tabs
                .iter()
                .any(|t| account_exists(&t.app_id, &t.account_id)),
        })
        .collect()
}

fn session_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("session.json"))
}

/// Load the saved session. Missing or corrupt → empty (nothing to restore).
pub fn load_session(app: &AppHandle) -> Session {
    let Ok(path) = session_path(app) else {
        return Session::default();
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return Session::default();
    };
    parse_session(&raw)
}

/// Forget the saved session (the Ask-mode "Dismiss" path).
pub fn clear_session(app: &AppHandle) {
    if let Ok(path) = session_path(app) {
        let _ = fs::remove_file(path);
    }
}

// ------------------------------------------------------------------
// Crash-loop sentinel (v0.9.7).
//
// Session restore auto-opens windows at launch; if the process dies in
// the restore window (the v0.9.6 crash loop), the next launch must NOT
// blindly restore again. `restore.inprogress` is armed when a restore
// becomes possible and deleted only after the app has survived startup
// (a grace period) or quit cleanly. A stale sentinel forces ask-mode
// for one launch with an honest note instead of looping.
// ------------------------------------------------------------------

fn sentinel_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("restore.inprogress"))
}

fn write_sentinel_at(path: &PathBuf) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, b"restore in progress");
}

/// true if a stale sentinel existed; consumes it either way.
fn take_sentinel_at(path: &PathBuf) -> bool {
    let existed = path.exists();
    let _ = fs::remove_file(path);
    existed
}

fn clear_sentinel_at(path: &PathBuf) {
    let _ = fs::remove_file(path);
}

/// One-time-per-process flag: a stale sentinel forced this launch into
/// ask-mode instead of auto-restoring. Managed as Tauri state in setup.
#[derive(Debug, Default)]
pub struct SessionRestoreForced(pub AtomicBool);

/// Call once, early in setup, after SessionRestoreForced is managed: a
/// stale sentinel (previous run died inside the restore window) forces
/// ask-mode for this launch; otherwise, when auto-restore is on, arm
/// the sentinel for this run.
pub fn check_startup_sentinel(app: &AppHandle) {
    let stale = sentinel_path(app).map(|p| take_sentinel_at(&p)).unwrap_or(false);
    if stale {
        if let Some(s) = app.try_state::<SessionRestoreForced>() {
            s.0.store(true, Ordering::SeqCst);
        }
        return;
    }
    let restore =
        crate::launcher_settings::load(app).startup_mode == StartupMode::Restore;
    if restore {
        if let Ok(p) = sentinel_path(app) {
            write_sentinel_at(&p);
        }
    }
}

/// Delete the sentinel: clean shutdown, or the grace period after a
/// restore (the app survived startup).
pub fn clear_restore_sentinel(app: &AppHandle) {
    if let Ok(p) = sentinel_path(app) {
        clear_sentinel_at(&p);
    }
}

/// The app must SURVIVE startup for the sentinel to clear: a crash any
/// time before this fires leaves the sentinel for the next launch's
/// forced ask-mode.
fn spawn_sentinel_grace(app: &AppHandle) {
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("appmaka-sentinel-grace".to_string())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            clear_restore_sentinel(&app);
        });
}

fn save_session(app: &AppHandle, session: &Session) {
    let Ok(path) = session_path(app) else {
        return;
    };
    let Ok(raw) = serde_json::to_string_pretty(session) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    // Atomic like the other JSON stores: crash mid-write can't corrupt it.
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, raw).is_err() {
        return;
    }
    let _ = fs::rename(&tmp, &path);
}

fn prev_account_rect(session: &Session, app_id: &str, account_id: &str) -> Option<WindowRect> {
    session.windows.iter().find_map(|w| match w {
        SessionWindow::Account {
            app_id: a,
            account_id: ac,
            rect,
            ..
        } if a == app_id && ac == account_id => rect.clone(),
        _ => None,
    })
}

fn prev_search_rect(session: &Session) -> Option<WindowRect> {
    session.windows.iter().find_map(|w| match w {
        SessionWindow::Search { rect, .. } => rect.clone(),
        _ => None,
    })
}

/// v0.10.0: last known rect of a tabbed group, by stable group id.
fn prev_tabbed_rect(session: &Session, id: &str) -> Option<WindowRect> {
    session.windows.iter().find_map(|w| match w {
        SessionWindow::Tabbed { id: gid, rect, .. } if gid == id => rect.clone(),
        _ => None,
    })
}

/// Current logical-pixel rect of a window. None when minimized (minimized
/// windows report junk coordinates — the caller keeps the last good
/// position) or when the geometry is unreadable.
fn live_window_rect(window: &tauri::WebviewWindow) -> Option<WindowRect> {
    match window.is_minimized() {
        Ok(false) => {}
        // Minimized or unknown: fail closed, keep the last good rect.
        _ => return None,
    }
    let scale = window.scale_factor().unwrap_or(0.0);
    let pos = window.outer_position().ok()?;
    let size = window.outer_size().ok()?;
    physical_to_logical_rect(pos.x, pos.y, size.width, size.height, scale)
}

/// Live search-window info for session ordering. Managed as Tauri state;
/// the window title is the display copy, this is the session copy.
/// (Public only because it rides in the public `SearchLiveState`; treat
/// both as session-module internals.)
#[derive(Debug, Clone)]
pub struct SearchLive {
    query: String,
    opened_at: u64,
}

/// Managed as Tauri state. Updated on search open/re-navigate, cleared on
/// close or failed build.
#[derive(Debug, Default)]
pub struct SearchLiveState(pub Mutex<Option<SearchLive>>);

/// Record a search-window open (or re-navigation, which keeps the original
/// open order and only updates the query).
pub fn note_search_opened(app: &AppHandle, query: &str) {
    let now = unix_millis();
    if let Some(state) = app.try_state::<SearchLiveState>() {
        if let Ok(mut guard) = state.0.lock() {
            match guard.as_mut() {
                Some(live) => live.query = query.to_string(),
                None => {
                    *guard = Some(SearchLive {
                        query: query.to_string(),
                        opened_at: now,
                    })
                }
            }
        }
    }
}

/// Drop the search-window session entry (close or failed build).
pub fn note_search_closed(app: &AppHandle) {
    if let Some(state) = app.try_state::<SearchLiveState>() {
        if let Ok(mut guard) = state.0.lock() {
            *guard = None;
        }
    }
}

/// The live search window's current query, if the window is open.
pub fn search_live_query(app: &AppHandle) -> Option<String> {
    app.try_state::<SearchLiveState>().and_then(|state| {
        state
            .0
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|live| live.query.clone()))
    })
}

/// Rewrite session.json from the live window set. Called on every
/// account-window open/close, search-window open/close/navigate, and
/// debounced move/resize. Cheap: one tiny atomic JSON write.
pub fn write_session(app: &AppHandle) {
    let prev = load_session(app);
    let mut accounts = Vec::new();
    for (label, app_id, account_id, opened_at) in windows::live_tracked_accounts(app) {
        let rect = merge_rect(
            app.get_webview_window(&label)
                .as_ref()
                .and_then(live_window_rect),
            prev_account_rect(&prev, &app_id, &account_id),
        );
        accounts.push((app_id, account_id, rect, opened_at));
    }
    let search = app
        .try_state::<SearchLiveState>()
        .and_then(|s| s.0.lock().ok().and_then(|g| g.clone()))
        .filter(|live| {
            !live.query.trim().is_empty()
                && app.get_webview_window(SEARCH_WINDOW_LABEL).is_some()
        })
        .map(|live| {
            let rect = merge_rect(
                app.get_webview_window(SEARCH_WINDOW_LABEL)
                    .as_ref()
                    .and_then(live_window_rect),
                prev_search_rect(&prev),
            );
            (live.query, rect, live.opened_at)
        });
    save_session(
        app,
        &build_session(
            accounts,
            search,
            // v0.10.0: live tabbed groups. Written with "now" as the
            // ordering key, so tabbed windows deterministically restore
            // after account windows.
            crate::tabs::live_tab_groups(app)
                .into_iter()
                .map(|g| {
                    let rect = merge_rect(
                        app.get_webview_window(&g.label)
                            .as_ref()
                            .and_then(live_window_rect),
                        prev_tabbed_rect(&prev, &g.id),
                    );
                    (g.id, g.tabs, g.active, rect, unix_millis())
                })
                .collect(),
            &|label| crate::pin::is_pinned(app, label),
        ),
    );
}

/// At most one pending debounced write: a move/resize storm collapses into
/// a single write ~500ms after the first event. The write reads live state,
/// so it always captures the final position.
static WRITE_PENDING: AtomicBool = AtomicBool::new(false);

pub fn schedule_session_write(app: &AppHandle) {
    if WRITE_PENDING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        WRITE_PENDING.store(false, Ordering::SeqCst);
        write_session(&app);
    });
}

fn logical_monitors(app: &AppHandle) -> Vec<LogicalMonitor> {
    app.available_monitors()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| {
            let scale = m.scale_factor();
            if !scale.is_finite() || scale <= 0.0 {
                return None;
            }
            let size = m.size();
            let pos = m.position();
            Some(LogicalMonitor {
                x: (pos.x as f64 / scale).round() as i32,
                y: (pos.y as f64 / scale).round() as i32,
                width: (size.width as f64 / scale).round().max(1.0) as u32,
                height: (size.height as f64 / scale).round().max(1.0) as u32,
            })
        })
        .collect()
}

/// Reopen every saved window in open order, each at its saved geometry
/// (validated against the current monitors; off-screen falls back to the
/// default centered placement). Best-effort: unknown accounts are skipped
/// silently and one window's failure never stops the rest. Returns how
/// many windows were (re)opened.
///
/// Fault isolation (v0.9.7): every window open runs under panic
/// isolation, and opens are staggered ~150ms so a burst of windows can't
/// thundering-herd the caption thread. A panicking or failing open counts
/// as a miss and the rest still open.
pub fn restore_session_now(app: &AppHandle) -> usize {
    restore_filtered(app, &|_| true)
}

/// Reopen only the pinned windows (v0.9.9): pinned windows always restore
/// on launch regardless of the "On startup" setting. Same isolation,
/// stagger, and sentinel machinery as a full restore.
pub fn restore_pinned_windows(app: &AppHandle) -> usize {
    restore_filtered(app, &|w| w.pinned())
}

fn restore_filtered(app: &AppHandle, keep: &dyn Fn(&SessionWindow) -> bool) -> usize {
    let session = load_session(app);
    if session.windows.is_empty() {
        return 0;
    }
    let (Some(store), Some(adblock), Some(winstate), Some(tabstate)) = (
        app.try_state::<AppStore>(),
        app.try_state::<AdblockState>(),
        app.try_state::<WindowState>(),
        app.try_state::<crate::tabs::TabState>(),
    ) else {
        return 0;
    };
    let account_exists = |app_id: &str, account_id: &str| {
        store
            .get(app_id)
            .map(|a| a.accounts.iter().any(|ac| ac.id == account_id))
            .unwrap_or(false)
    };
    let monitors = logical_monitors(app);
    let restorable: Vec<&SessionWindow> = partition_restorable(&session, &account_exists)
        .into_iter()
        .filter(|w| keep(w))
        .collect();
    // TEST-ONLY fault injection (v0.9.7, debug builds only):
    // APPMAKA_TEST_PANIC_ON_OPEN=N makes the Nth restore window open
    // panic, simulating the Windows caption-strip crash so the
    // containment + sentinel machinery can be verified E2E on Linux.
    // Release builds never contain this code.
    #[cfg(debug_assertions)]
    let mut fault_n = 0usize;
    let mut open_one = |w: &SessionWindow| -> bool {
        open_isolated(|| {
            #[cfg(debug_assertions)]
            {
                fault_n += 1;
                if let Ok(target) = std::env::var("APPMAKA_TEST_PANIC_ON_OPEN") {
                    if target.parse::<usize>().ok() == Some(fault_n) {
                        panic!(
                            "test fault injection: panicking on restore window open #{fault_n}"
                        );
                    }
                }
            }
            match w {
            SessionWindow::Account {
                app_id,
                account_id,
                rect,
                ..
            } => {
                let placement = placement_for_rect(rect.as_ref(), &monitors);
                windows::open_account_placed(
                    app, &store, &adblock, &winstate, app_id, account_id, placement,
                )
            }
            SessionWindow::Search { query, rect, .. } => {
                let placement = placement_for_rect(rect.as_ref(), &monitors);
                crate::websearch::open_search_window_placed(app, &adblock, query, placement)
            }
            // v0.10.0: tabbed windows restore with their saved tab set;
            // the active tab opens live, the rest are lazy (nothing
            // exists until first click). Dead tabs are dropped by the
            // open call itself.
            SessionWindow::Tabbed {
                id,
                tabs,
                active,
                rect,
                ..
            } => {
                let placement = placement_for_rect(rect.as_ref(), &monitors);
                crate::tabs::open_tabbed_window(
                    app,
                    &store,
                    &adblock,
                    &tabstate,
                    crate::tabs::OpenTabbedParams {
                        initial: tabs.clone(),
                        active: *active,
                        placement,
                        restore_id: Some(id.clone()),
                    },
                )
                .map(|_| ())
            }
            }
        })
    };
    restore_entries(&restorable, &mut open_one, &mut || {
        std::thread::sleep(Duration::from_millis(150))
    })
}

/// One window-open under panic isolation: a panicking open counts as a
/// failure and never propagates. (A panic on the restore thread would
/// only kill the thread, but isolation keeps the remaining windows
/// opening and the accounting honest.)
fn open_isolated(f: impl FnOnce() -> Result<(), String>) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .map(|r| r.is_ok())
        .unwrap_or(false)
}

/// Open entries in open order with a stagger between them (no thundering
/// herd on window creation), each via the caller's open callback. Pure
/// over callbacks so ordering and isolation are unit-testable; the real
/// stagger is 150ms.
fn restore_entries(
    entries: &[&SessionWindow],
    open_one: &mut dyn FnMut(&SessionWindow) -> bool,
    stagger: &mut dyn FnMut(),
) -> usize {
    let mut opened = 0;
    for (i, w) in entries.iter().enumerate() {
        if i > 0 {
            stagger();
        }
        if open_one(w) {
            opened += 1;
        }
    }
    opened
}

/// Manual restore (launcher "Restore session" button / ask banner): the
/// same sentinel + grace as auto-restore, so a crash here also forces
/// ask-mode next launch instead of looping.
pub fn restore_session_manual(app: &AppHandle) -> usize {
    if let Ok(p) = sentinel_path(app) {
        write_sentinel_at(&p);
    }
    let n = restore_session_now(app);
    spawn_sentinel_grace(app);
    n
}

/// Restore on launch when the user chose "Restore last session".
/// Best-effort and silent: startup never waits on it and never fails on it.
///
/// Crash-loop guard (v0.9.7): a stale sentinel from a previous run forces
/// ask-mode for this launch (set at startup) — auto-restore never runs
/// into the same crash twice. The per-window isolation in
/// restore_session_now keeps one bad window from stopping the rest, and
/// the launcher is independent: it always reaches a usable state.
///
/// Pinned windows (v0.9.9) always restore on launch regardless of the "On
/// startup" setting — the pin is the user's explicit "keep this open".
/// The stale-sentinel guard outranks the pin: after a crash, pinned
/// windows go through ask-mode like everything else.
pub fn maybe_restore_on_launch(app: &AppHandle) {
    let forced = app
        .try_state::<SessionRestoreForced>()
        .map(|s| s.0.load(Ordering::SeqCst))
        .unwrap_or(false);
    if forced {
        return;
    }
    let restore = app
        .try_state::<Mutex<LauncherSettings>>()
        .and_then(|s| s.lock().ok().map(|s| s.startup_mode == StartupMode::Restore))
        .unwrap_or(false);
    if restore {
        let n = restore_session_now(app);
        if n > 0 {
            eprintln!("[appmaka] restored {n} window(s) from last session");
        }
    } else if crate::pin::pinned_restore_applies(forced, restore) {
        // Not in restore mode, but pinned windows come back anyway — unless
        // the crash-loop sentinel forced ask-mode (it outranks the pin).
        let n = restore_pinned_windows(app);
        if n > 0 {
            eprintln!("[appmaka] restored {n} pinned window(s)");
        }
    }
    spawn_sentinel_grace(app);
}

/// One-time-per-process flag for the "Ask me" offer.
#[derive(Debug, Default)]
pub struct SessionAskConsumed(pub AtomicBool);

/// The Ask-mode offer shown on the launcher: how many windows, a few
/// human-readable names, whether a search is among them. One-time per
/// process — returns None once consumed, when the mode isn't Ask (and no
/// stale sentinel forced ask-mode), or when the saved session is empty.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRestoreOffer {
    pub window_count: usize,
    pub names: Vec<String>,
    pub has_search: bool,
    /// True when a stale sentinel forced this launch into ask-mode: the
    /// previous run died inside the restore window, so the UI explains
    /// why it didn't auto-restore.
    pub stale_restore: bool,
}

/// Which saved windows the Ask-mode offer covers. Pinned windows restore
/// directly without asking, so the offer excludes them — no double-count,
/// no asking about them. When a stale sentinel forced ask-mode, pinned
/// windows did NOT auto-restore (the guard outranks the pin), so they
/// stay in the offer. Pure so the rule is unit-testable.
pub fn offer_entries(windows: &[SessionWindow], forced: bool) -> Vec<&SessionWindow> {
    windows
        .iter()
        .filter(|w| forced || !w.pinned())
        .collect()
}

pub fn take_restore_offer(app: &AppHandle) -> Option<SessionRestoreOffer> {
    let consumed = app.try_state::<SessionAskConsumed>()?;
    if consumed.0.swap(true, Ordering::SeqCst) {
        return None;
    }
    let ask = app
        .try_state::<Mutex<LauncherSettings>>()
        .and_then(|s| s.lock().ok().map(|s| s.startup_mode == StartupMode::Ask))
        .unwrap_or(false);
    // A stale sentinel forces ask behavior for one launch even when the
    // setting is "Restore last session": better to ask than to loop.
    let forced = app
        .try_state::<SessionRestoreForced>()
        .map(|s| s.0.load(Ordering::SeqCst))
        .unwrap_or(false);
    if !ask && !forced {
        return None;
    }
    let session = load_session(app);
    if session.windows.is_empty() {
        return None;
    }
    // Pinned windows (v0.9.9) restore directly without asking, so the
    // offer excludes them — no double-count, no asking about them. But
    // when a stale sentinel forced ask-mode, pinned windows did NOT
    // auto-restore (the guard outranks the pin), so they stay in the offer
    // and the user can still bring them back with one click.
    let offered: Vec<&SessionWindow> = offer_entries(&session.windows, forced);
    if offered.is_empty() {
        return None;
    }
    let store = app.try_state::<AppStore>();
    let mut names = Vec::new();
    let mut has_search = false;
    for w in &offered {
        if names.len() >= 5 {
            break;
        }
        match w {
            SessionWindow::Account { app_id, account_id, .. } => {
                let name = store
                    .as_deref()
                    .and_then(|s| s.get(app_id).ok())
                    .and_then(|a| {
                        a.accounts
                            .iter()
                            .find(|ac| ac.id == *account_id)
                            .map(|ac| format!("{} — {}", a.name, ac.label))
                    })
                    .unwrap_or_else(|| "An account".to_string());
                names.push(name);
            }
            SessionWindow::Search { query, .. } => {
                has_search = true;
                let short: String = query.chars().take(32).collect();
                names.push(format!("Search: \"{short}\""));
            }
            // v0.10.0: name the first few tabs so the offer reads like
            // the account entries above it.
            SessionWindow::Tabbed { tabs, .. } => {
                let mut tab_names: Vec<String> = Vec::new();
                if let Some(s) = store.as_deref() {
                    for t in tabs.iter().take(3) {
                        if let Ok(a) = s.get(&t.app_id) {
                            tab_names.push(a.name.clone());
                        }
                    }
                }
                if tab_names.is_empty() {
                    names.push(format!("Tabbed window ({} tabs)", tabs.len()));
                } else {
                    names.push(format!("Tabbed: {}", tab_names.join(", ")));
                }
            }
        }
    }
    Some(SessionRestoreOffer {
        window_count: offered.len(),
        names,
        has_search,
        stale_restore: forced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: u32, h: u32) -> Option<WindowRect> {
        Some(WindowRect {
            x,
            y,
            width: w,
            height: h,
        })
    }

    #[test]
    fn session_round_trips() {
        let session = build_session(
            vec![
                ("app1".into(), "acc1".into(), rect(10, 20, 800, 600), 100),
                ("app2".into(), "acc2".into(), None, 50),
            ],
            Some(("hello".into(), rect(0, 0, 1200, 800), 75)),
            vec![],
            &|_| false,
        );
        // Sorted by open time regardless of input order.
        assert_eq!(session.windows.len(), 3);
        let raw = serde_json::to_string(&session).unwrap();
        // camelCase keys on the wire.
        assert!(raw.contains("\"appId\""));
        assert!(raw.contains("\"accountId\""));
        let back = parse_session(&raw);
        assert_eq!(back, session);
    }

    #[test]
    fn corrupt_file_yields_empty_session() {
        assert_eq!(parse_session(""), Session::default());
        assert_eq!(parse_session("{oops"), Session::default());
        assert_eq!(parse_session("null"), Session::default());
    }

    #[test]
    fn bad_entries_are_skipped_not_fatal() {
        let raw = r#"{"windows":[
            {"kind":"account","appId":"a","accountId":"b","rect":{"x":1,"y":2,"width":3,"height":4}},
            {"kind":"bogus","x":1},
            {"kind":"account","appId":"","accountId":"b"},
            {"kind":"search","query":"   "},
            {"kind":"search","query":"ok"}
        ]}"#;
        let s = parse_session(raw);
        assert_eq!(s.windows.len(), 2);
    }

    #[test]
    fn physical_to_logical_scale_conversion() {
        assert_eq!(
            physical_to_logical_rect(0, 0, 3840, 2160, 2.0),
            rect(0, 0, 1920, 1080)
        );
        assert_eq!(
            physical_to_logical_rect(100, 100, 1250, 800, 1.25),
            rect(80, 80, 1000, 640)
        );
        assert_eq!(physical_to_logical_rect(0, 0, 100, 100, 0.0), None);
        assert_eq!(physical_to_logical_rect(0, 0, 100, 100, -1.0), None);
        assert_eq!(
            physical_to_logical_rect(0, 0, 100, 100, f64::NAN),
            None
        );
    }

    #[test]
    fn off_screen_rect_is_not_visible() {
        let monitors = vec![LogicalMonitor {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        // On-screen.
        assert!(rect_visible_on_any(
            &rect(100, 100, 800, 600).unwrap(),
            &monitors
        ));
        // Partially overlapping still counts (a sliver is visible).
        assert!(rect_visible_on_any(
            &rect(1800, 900, 800, 600).unwrap(),
            &monitors
        ));
        // Fully off-screen (monitor unplugged since last run).
        assert!(!rect_visible_on_any(
            &rect(3000, 100, 800, 600).unwrap(),
            &monitors
        ));
        assert!(!rect_visible_on_any(
            &rect(-1000, -1000, 800, 600).unwrap(),
            &monitors
        ));
        // Can't validate → trust it.
        assert!(rect_visible_on_any(
            &rect(3000, 100, 800, 600).unwrap(),
            &[]
        ));
    }

    #[test]
    fn placement_falls_back_centered_off_screen() {
        let monitors = vec![LogicalMonitor {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        assert!(placement_for_rect(None, &monitors).is_none());
        assert!(placement_for_rect(rect(3000, 100, 800, 600).as_ref(), &monitors).is_none());
        let p = placement_for_rect(rect(100, 100, 800, 600).as_ref(), &monitors).unwrap();
        assert_eq!((p.x, p.y, p.width, p.height), (100.0, 100.0, 800.0, 600.0));
    }

    #[test]
    fn minimized_window_keeps_last_good_rect() {
        let good = rect(10, 20, 800, 600);
        // Minimized (live unreadable) → keep the saved position.
        assert_eq!(merge_rect(None, good.clone()), good);
        // Fresh live rect wins.
        assert_eq!(merge_rect(rect(1, 2, 3, 4), good.clone()), rect(1, 2, 3, 4));
        // Nothing known → default placement.
        assert_eq!(merge_rect(None, None), None);
    }

    #[test]
    fn unknown_accounts_are_skipped_silently() {
        let session = build_session(
            vec![
                ("app1".into(), "gone".into(), None, 1),
                ("app1".into(), "here".into(), None, 2),
            ],
            Some(("q".into(), None, 3)),
            vec![],
            &|_| false,
        );
        let exists = |app_id: &str, account_id: &str| app_id == "app1" && account_id == "here";
        let kept = partition_restorable(&session, &exists);
        assert_eq!(kept.len(), 2);
        assert!(matches!(kept[0], SessionWindow::Account { account_id, .. } if account_id == "here"));
        assert!(matches!(kept[1], SessionWindow::Search { .. }));
    }

    fn sentinel_test_path() -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("appmaka-sentinel-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        dir.join("restore.inprogress")
    }

    #[test]
    fn sentinel_state_machine() {
        let path = sentinel_test_path();
        let _ = fs::remove_file(&path);
        // Fresh start: nothing there.
        assert!(!take_sentinel_at(&path));
        // Crash mid-restore: sentinel armed, never cleared.
        write_sentinel_at(&path);
        assert!(path.exists());
        // Next launch: stale → true, and consumed either way.
        assert!(take_sentinel_at(&path));
        assert!(!path.exists());
        // Clean exit: armed then cleared → next launch sees nothing.
        write_sentinel_at(&path);
        clear_sentinel_at(&path);
        assert!(!take_sentinel_at(&path));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn panicking_window_open_is_contained() {
        assert!(!open_isolated(|| -> Result<(), String> { panic!("boom") }));
        assert!(open_isolated(|| Ok(())));
        assert!(!open_isolated(|| Err("nope".to_string())));
    }

    #[test]
    fn restore_entries_preserves_order_and_staggers() {
        let session = build_session(
            vec![
                ("a".into(), "1".into(), None, 3),
                ("a".into(), "2".into(), None, 1),
                ("a".into(), "3".into(), None, 2),
            ],
            None,
            vec![],
            &|_| false,
        );
        let exists = |_: &str, _: &str| true;
        let entries = partition_restorable(&session, &exists);
        let mut seen = Vec::new();
        let mut pauses = 0;
        let mut open_one = |w: &SessionWindow| -> bool {
            if let SessionWindow::Account { account_id, .. } = w {
                seen.push(account_id.clone());
            }
            true
        };
        let n = restore_entries(&entries, &mut open_one, &mut || pauses += 1);
        assert_eq!(n, 3);
        // Open order follows the session's open order, not input order.
        assert_eq!(seen, vec!["2".to_string(), "3".to_string(), "1".to_string()]);
        // Stagger runs between windows only: n-1 pauses for n entries.
        assert_eq!(pauses, 2);
        // A panicking entry doesn't stop the rest.
        let mut seen2 = Vec::new();
        let mut open_flaky = |w: &SessionWindow| -> bool {
            if let SessionWindow::Account { account_id, .. } = w {
                seen2.push(account_id.clone());
            }
            open_isolated(|| -> Result<(), String> {
                if seen2.len() == 2 {
                    panic!("mid-restore boom");
                }
                Ok(())
            })
        };
        let n2 = restore_entries(&entries, &mut open_flaky, &mut || {});
        assert_eq!(n2, 2);
        assert_eq!(seen2.len(), 3);
    }

    #[test]
    fn pinned_flag_round_trips_and_defaults_false() {
        // New writes carry the flag.
        let session = build_session(
            vec![("app1".into(), "acc1".into(), None, 1)],
            Some(("q".into(), None, 2)),
            vec![],
            &|label| label == "acct-app1-acc1",
        );
        assert!(session.windows[0].pinned());
        assert!(!session.windows[1].pinned());
        let raw = serde_json::to_string(&session).unwrap();
        assert!(raw.contains("\"pinned\":true"));
        let back = parse_session(&raw);
        assert_eq!(back, session);
        // Old files without the flag load as unpinned — no migration.
        let old = r#"{"windows":[
            {"kind":"account","appId":"a","accountId":"b"},
            {"kind":"search","query":"q"}
        ]}"#;
        let s = parse_session(old);
        assert_eq!(s.windows.len(), 2);
        assert!(s.windows.iter().all(|w| !w.pinned()));
        // A non-bool pinned value fails that entry's parse; the entry is
        // skipped like any other corrupt entry.
        let bad = r#"{"windows":[
            {"kind":"account","appId":"a","accountId":"b","pinned":"yes"},
            {"kind":"search","query":"ok"}
        ]}"#;
        let s = parse_session(bad);
        assert_eq!(s.windows.len(), 1);
    }

    #[test]
    fn offer_entries_excludes_pinned_unless_forced() {
        let session = build_session(
            vec![
                ("app1".into(), "keep".into(), None, 1),
                ("app1".into(), "askme".into(), None, 2),
            ],
            None,
            vec![],
            &|label| label == "acct-app1-keep",
        );
        // Normal ask-mode: pinned entries restore directly, offer skips them.
        let offered = offer_entries(&session.windows, false);
        assert_eq!(offered.len(), 1);
        assert!(matches!(
            offered[0],
            SessionWindow::Account { account_id, .. } if account_id == "askme"
        ));
        // Stale sentinel forced ask-mode: the pin didn't auto-restore, so
        // the offer keeps the pinned entry.
        let offered = offer_entries(&session.windows, true);
        assert_eq!(offered.len(), 2);
    }
}
