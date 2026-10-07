//! Tabbed app windows (v0.10.0): stack multiple apps in ONE window with a
//! tab strip across the top bar. Clicking a tab shows that app; the tab set
//! is user-chosen and saved across restarts.
//!
//! Light-RAM design (the user's choice): ONE live webview per tabbed window.
//! Switching tabs closes and rebuilds the `WebviewWindow` with the new
//! tab's existing `data_directory` — the same per-app session dirs
//! `windows.rs` already uses — so each tab keeps its own login session
//! exactly like separate windows do today. Switching reloads the page;
//! there are no background-live tabs.
//!
//! Stable Tauri API only: `Window::add_child` (multi-webview) needs the
//! `unstable` cargo feature, which this repo deliberately does not use, so
//! there is no in-place webview swap — the whole window is rebuilt.
//!
//! Window labels are generation-suffixed (`tabbed-{group}-g{gen}`): closing
//! is asynchronous, so rebuilding under the same label could collide with
//! the dying window. TabState maps group id -> current label; nothing
//! depends on label stability.
//!
//! Threading: window building happens in `async` tauri commands (the
//! `open_account` pattern) or on a dedicated spawned thread when triggered
//! from the Win32 caption strip — never inside the window proc, never on a
//! sync IPC thread, never on the main thread (AGENTS.md deadlock rules).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::session::WindowRect;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

use crate::adblock::AdblockState;
use crate::store::AppStore;
use crate::windows::WindowPlacement;

/// Label prefix for tabbed page windows (generation-suffixed).
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub fn is_tabbed_label(label: &str) -> bool {
    label.starts_with("tabbed-")
}

/// Stable pin key for a tab group. pin.rs is label-keyed, but the tabbed
/// window's label changes on every tab switch — the group id never does.
pub fn pin_key(group_id: &str) -> String {
    format!("tabbed:{group_id}")
}

fn window_label(group_id: &str, generation: u64) -> String {
    format!("tabbed-{group_id}-g{generation}")
}

/// One tab: an (app, account) pair. `last_url` is runtime-only (never
/// persisted): the page the tab was showing when the user switched away,
/// so switching back reopens where they left off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabEntry {
    #[serde(rename = "appId", default)]
    pub app_id: String,
    #[serde(rename = "accountId", default)]
    pub account_id: String,
    #[serde(skip, default)]
    pub last_url: Option<String>,
}

impl TabEntry {
    pub fn valid(&self) -> bool {
        !self.app_id.is_empty() && !self.account_id.is_empty()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TabGroup {
    id: String,
    /// Current live window label (`tabbed-{id}-g{gen}`).
    label: String,
    tabs: Vec<TabEntry>,
    active: usize,
    generation: u64,
    /// Last fetched theme-color hex (`#rrggbb`) of the ACTIVE tab, for the
    /// strip tint. None until the first successful fetch.
    tint: Option<String>,
}

/// Live per-group bookkeeping, managed as Tauri state.
#[derive(Default)]
pub struct TabState {
    inner: Mutex<HashMap<String, TabGroup>>,
}

static GROUP_SEQ: AtomicU64 = AtomicU64::new(1);

fn new_group_id() -> String {
    let n = GROUP_SEQ.fetch_add(1, Ordering::SeqCst);
    format!("g{}-{}", std::process::id(), n)
}

// ---------------------------------------------------------------------------
// Pure logic: unit-tested on every platform
// ---------------------------------------------------------------------------

/// Pure decision: does switching to `index` require rebuilding the live
/// webview? The live window always shows `active` — except right after the
/// first tab lands in an empty group (`force`), when the window is still
/// on about:blank and must be rebuilt to load the tab.
fn switch_needs_rebuild(active: usize, index: usize, force: bool) -> bool {
    force || index != active
}

/// Keyboard tab-switch action from the Windows low-level hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub enum TabKeyAction {
    Next,
    Prev,
    Index(usize),
}

/// Pure classifier for Ctrl+Tab / Ctrl+Shift+Tab / Ctrl+1..9. Only fires
/// with Ctrl held; everything else passes through untouched.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub fn tab_key_action(vk_code: u32, ctrl_held: bool, shift_held: bool) -> Option<TabKeyAction> {
    if !ctrl_held {
        return None;
    }
    match vk_code {
        // VK_TAB
        0x09 => Some(if shift_held {
            TabKeyAction::Prev
        } else {
            TabKeyAction::Next
        }),
        // '1'..'9'
        0x31..=0x39 => Some(TabKeyAction::Index((vk_code - 0x31) as usize)),
        _ => None,
    }
}

/// Wrap-around tab index math: negative deltas wrap from the first tab to
/// the last, oversized deltas wrap repeatedly.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub fn wrap_index(active: usize, n: usize, delta: isize) -> usize {
    if n == 0 {
        return 0;
    }
    ((active as isize + delta).rem_euclid(n as isize)) as usize
}

/// Resolve a TabKeyAction against the current tab set: None when there is
/// nothing to do (single tab + Next/Prev, or an out-of-range number key).
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub fn resolve_key_action(action: TabKeyAction, active: usize, n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let target = match action {
        TabKeyAction::Next => wrap_index(active, n, 1),
        TabKeyAction::Prev => wrap_index(active, n, -1),
        TabKeyAction::Index(i) => {
            if i >= n {
                return None;
            }
            i
        }
    };
    if target == active {
        return None;
    }
    Some(target)
}

/// True when light glyphs read better on the tint (dark background).
/// ITU-R BT.601 luma; the 128 midpoint is the honest boundary.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub fn tint_wants_light_text(r: u8, g: u8, b: u8) -> bool {
    let luma = 0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64;
    luma < 128.0
}

// ---------------------------------------------------------------------------
// Tab strip layout + hit-testing (pure; the Win32 strip and the Linux HTML
// strip share these numbers so both platforms behave the same)
// ---------------------------------------------------------------------------

/// Logical-pixel strip metrics, shared by the native and HTML strips.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub mod layout {
    /// Strip height, matching the caption strip's BAR_H_LOGICAL.
    pub const STRIP_H: f64 = 30.0;
    /// Min/max/close button slot width, matching BTN_W_LOGICAL.
    pub const BTN_W: f64 = 46.0;
    /// "+" button width.
    pub const ADD_W: f64 = 34.0;
    /// Tab width clamp.
    pub const TAB_MIN_W: f64 = 64.0;
    pub const TAB_MAX_W: f64 = 168.0;
    /// Tabbed strips carry a window close (X) button the page strips lack:
    /// an empty group has no tab to close, so the window needs its own X.
    pub const WINDOW_BUTTONS: f64 = 3.0 * BTN_W; // X, max, min

    /// Per-tab x-ranges (logical px) plus the "+" button's x offset, given
    /// the strip width. Tabs share the free area evenly within the clamp;
    /// when they overflow at minimum width they simply run past the buttons
    /// (v1: no scrolling; realistic tab counts fit).
    pub fn tab_ranges(strip_w: f64, n_tabs: usize) -> (Vec<(f64, f64)>, f64) {
        if n_tabs == 0 {
            return (Vec::new(), 0.0);
        }
        let free = (strip_w - WINDOW_BUTTONS - ADD_W).max(0.0);
        let w = (free / n_tabs as f64).clamp(TAB_MIN_W, TAB_MAX_W);
        let ranges = (0..n_tabs)
            .map(|i| (i as f64 * w, (i + 1) as f64 * w))
            .collect();
        (ranges, n_tabs as f64 * w)
    }
}

/// What a strip click hit. Mirrors `layout` so both strips agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub enum TabHit {
    Tab(usize),
    CloseTab(usize),
    Add,
    CloseWindow,
    Min,
    Max,
    Drag,
}

/// Pure hit-test in logical pixels. `close_w` is the width of the per-tab
/// close affordance at the tab's right edge.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub fn tab_hit_test(
    x: f64,
    y: f64,
    strip_w: f64,
    n_tabs: usize,
    close_w: f64,
) -> TabHit {
    use layout::*;
    if y < 0.0 || y >= STRIP_H || x < 0.0 || x >= strip_w {
        return TabHit::Drag;
    }
    // Right-side window buttons: X, max, min (rightmost).
    if x >= strip_w - BTN_W {
        return TabHit::Min;
    }
    if x >= strip_w - 2.0 * BTN_W {
        return TabHit::Max;
    }
    if x >= strip_w - 3.0 * BTN_W {
        return TabHit::CloseWindow;
    }
    let (ranges, add_x) = tab_ranges(strip_w, n_tabs);
    if x >= add_x && x < add_x + ADD_W {
        return TabHit::Add;
    }
    for (i, (x0, x1)) in ranges.iter().enumerate() {
        if x >= *x0 && x < *x1 {
            if x >= x1 - close_w {
                return TabHit::CloseTab(i);
            }
            return TabHit::Tab(i);
        }
    }
    TabHit::Drag
}

// ---------------------------------------------------------------------------
// Public view types (serialized to the dashboard / Linux strip)
// ---------------------------------------------------------------------------

/// One tab's display data.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TabSummary {
    pub app_id: String,
    pub account_id: String,
    pub app_name: String,
    pub account_label: String,
}

/// Full tab-group state, returned by the tab commands.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TabInfo {
    pub group_id: String,
    pub tabs: Vec<TabSummary>,
    pub active: usize,
    pub tint: Option<String>,
    pub group_closed: bool,
}

/// One dashboard row per open tabbed window.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TabbedWindowInfo {
    pub group_id: String,
    pub label: String,
    pub tabs: Vec<TabSummary>,
    pub active: usize,
    pub tint: Option<String>,
    pub focused: bool,
    pub pinned: bool,
}

fn tab_summary(store: &AppStore, entry: &TabEntry) -> TabSummary {
    let (app_name, account_label) = store
        .get(&entry.app_id)
        .ok()
        .and_then(|a| {
            a.accounts
                .into_iter()
                .find(|ac| ac.id == entry.account_id)
                .map(|ac| (a.name, ac.label))
        })
        .unwrap_or_else(|| ("Unknown app".to_string(), String::new()));
    TabSummary {
        app_id: entry.app_id.clone(),
        account_id: entry.account_id.clone(),
        app_name,
        account_label,
    }
}

fn tab_info(store: &AppStore, group: &TabGroup, group_closed: bool) -> TabInfo {
    TabInfo {
        group_id: group.id.clone(),
        tabs: group.tabs.iter().map(|t| tab_summary(store, t)).collect(),
        active: group.active,
        tint: group.tint.clone(),
        group_closed,
    }
}

// ---------------------------------------------------------------------------
// Window building
// ---------------------------------------------------------------------------

/// Logical-pixel geometry saved across a tab-switch rebuild.
struct SavedGeometry {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    maximized: bool,
}

/// Read the window's geometry for a rebuild. None while minimized —
/// minimized windows report junk coordinates (same rule as session.rs).
fn save_geometry(window: &WebviewWindow) -> Option<SavedGeometry> {
    if window.is_minimized().unwrap_or(false) {
        return None;
    }
    let maximized = window.is_maximized().unwrap_or(false);
    let scale = window.scale_factor().unwrap_or(1.0);
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let pos = window.outer_position().ok()?;
    let size = window.outer_size().ok()?;
    Some(SavedGeometry {
        x: pos.x as f64 / scale,
        y: pos.y as f64 / scale,
        w: size.width as f64 / scale,
        h: size.height as f64 / scale,
        maximized,
    })
}

/// Per-tab page context: URL + session dir + popup policy, resolved from
/// the store. None = the empty group state (about:blank in a throwaway
/// group profile; the user adds the first tab via +).
struct TabPage {
    url: url::Url,
    session_dir: std::path::PathBuf,
    title: String,
    popup: Option<crate::windows::PopupContext>,
    adblock_css: String,
}

fn resolve_tab_page(
    app: &AppHandle,
    store: &AppStore,
    adblock: &AdblockState,
    entry: Option<&TabEntry>,
    group_id: &str,
) -> Result<TabPage, String> {
    match entry {
        Some(e) => {
            let web_app = store.get(&e.app_id)?;
            let account = web_app
                .accounts
                .iter()
                .find(|a| a.id == e.account_id)
                .ok_or_else(|| "Account not found.".to_string())?;
            // last_url wins: reopen the tab where the user left it.
            let url_str = e.last_url.as_deref().unwrap_or(&web_app.url);
            let url: url::Url = url_str
                .parse()
                .map_err(|_| format!("App URL is not valid: {}", web_app.url))?;
            let session_dir = store.session_dir_for(&e.app_id, &e.account_id)?;
            let popup_policy = account
                .popup_policy
                .clone()
                .unwrap_or_else(|| web_app.settings.popup_policy.clone());
            Ok(TabPage {
                url,
                title: format!("{} — {}", web_app.name, account.label),
                popup: Some(crate::windows::PopupContext {
                    app: app.clone(),
                    app_id: e.app_id.clone(),
                    account_id: e.account_id.clone(),
                    app_name: web_app.name.clone(),
                    app_url: web_app.url.clone(),
                    session_dir: session_dir.clone(),
                    popup_policy,
                    popup_allowlist: web_app.settings.popup_allowlist.clone(),
                }),
                adblock_css: adblock.cosmetic_css_for(&web_app.url),
                session_dir,
            })
        }
        None => {
            let dir = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("could not resolve app data dir: {e}"))?
                .join("tabbed-profiles")
                .join(group_id);
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("could not create tabbed profile dir: {e}"))?;
            Ok(TabPage {
                url: url::Url::parse("about:blank").expect("about:blank parses"),
                title: "Tabbed window".to_string(),
                popup: None,
                adblock_css: String::new(),
                session_dir: dir,
            })
        }
    }
}

/// Arguments for `build_tabbed_window` (clippy: too_many_arguments).
struct BuildTabbedArgs<'a> {
    label: &'a str,
    entry: Option<&'a TabEntry>,
    group_id: &'a str,
    placement: Option<WindowPlacement>,
    geometry: Option<SavedGeometry>,
}

/// Build the page window for one tab. The caller owns TabState updates,
/// strip attach, and session writes.
fn build_tabbed_window(
    app: &AppHandle,
    store: &AppStore,
    adblock: &AdblockState,
    args: BuildTabbedArgs<'_>,
) -> Result<WebviewWindow, String> {
    let page = resolve_tab_page(app, store, adblock, args.entry, args.group_id)?;
    let mut builder = WebviewWindowBuilder::new(app, args.label, WebviewUrl::External(page.url.clone()))
        .data_directory(page.session_dir.clone())
        .title(&page.title);
    #[cfg(windows)]
    {
        // Frameless like account windows: the tab strip replaces the
        // native title bar (caption.rs paints the tabs).
        builder = builder.decorations(false);
    }
    if let Some(g) = args.geometry {
        if g.maximized {
            builder = builder.maximized(true);
        } else {
            builder = builder
                .position(g.x, g.y)
                .inner_size(g.w, g.h);
        }
    } else if let Some(p) = args.placement {
        builder = builder.position(p.x, p.y).inner_size(p.width, p.height);
    } else {
        builder = builder.inner_size(1200.0, 800.0).center();
    }
    let window_app = app.clone();
    match page.popup {
        Some(ctx) => {
            builder = builder.on_new_window(crate::windows::make_popup_handler(ctx));
        }
        None => {
            // Empty group: deny everything; there is no app context yet.
            builder = builder.on_new_window(|_url: url::Url, _f| {
                tauri::webview::NewWindowResponse::Deny
            });
        }
    }
    builder = builder
        .on_download(crate::downloads::make_download_handler(window_app.clone()))
        .initialization_script(crate::windows::TARGET_BLANK_SHIM_JS)
        .initialization_script(crate::windows::NAV_KEYS_JS);
    if !page.adblock_css.is_empty() {
        builder = builder.initialization_script(crate::windows::cosmetic_init_script(&page.adblock_css));
    }
    // v0.10.0: re-tint the strip when the active tab navigates
    // (theme-color can change per page). Same pattern as the v0.9.11
    // popup retint: never block the navigation decision, detached thread.
    #[cfg(windows)]
    {
        let nav_app = window_app.clone();
        let nav_label = args.label.to_string();
        builder = builder.on_navigation(move |nav_url: &url::Url| {
            let app = nav_app.clone();
            let lbl = nav_label.clone();
            let u = nav_url.clone();
            std::thread::Builder::new()
                .name(format!("appmaka-tabbed-retint-{lbl}"))
                .spawn(move || tint_tabbed_caption(&app, &lbl, &u))
                .ok();
            true
        });
    }
    let window = builder
        .build()
        .map_err(|e| format!("could not open tabbed window: {e}"))?;
    #[cfg(windows)]
    let _ = window.set_shadow(true);

    // Window events: strip sync (both platforms) + destroy cleanup.
    // The closure only locks TabState briefly and issues no re-entrant
    // Win32 calls under the lock.
    let ev_app = app.clone();
    let ev_label = args.label.to_string();
    #[cfg(not(windows))]
    let ev_group = args.group_id.to_string();
    window.on_window_event(move |event| {
        match event {
            WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
                #[cfg(windows)]
                crate::caption::page_window_moved(&ev_label);
                #[cfg(not(windows))]
                reposition_strip_window(&ev_app, &ev_group);
            }
            WindowEvent::Focused(_focused) => {
                // Linux strip minimize-sync: a minimized page loses
                // focus, so hide the strip only when the page reports
                // itself minimized (plain focus loss keeps the strip).
                #[cfg(not(windows))]
                {
                    if *_focused {
                        set_strip_visible(&ev_app, &ev_group, true);
                    } else if ev_app
                        .get_webview_window(&ev_label)
                        .and_then(|w| w.is_minimized().ok())
                        .unwrap_or(false)
                    {
                        set_strip_visible(&ev_app, &ev_group, false);
                    }
                }
            }
            WindowEvent::Destroyed => {
                on_tabbed_window_destroyed(&ev_app, &ev_label);
            }
            _ => {}
        }
    });
    Ok(window)
}

/// Attach the platform tab strip to a freshly built page window.
/// `reposition` is false on a tab switch: the new window reuses the old
/// geometry, so the Linux strip is already in the right place — and
/// querying the new window's position while the old one is being torn
/// down can hang (outer_position blocks on the event loop).
fn attach_strip(
    app: &AppHandle,
    store: &AppStore,
    group: &TabGroup,
    window: &WebviewWindow,
    reposition: bool,
) {
    #[cfg(windows)]
    {
        let _ = reposition;
        let snapshot = tab_strip_snapshot(store, group);
        crate::caption::tabbed_window_opened(app, &group.label, window, snapshot);
        // Initial tint for the active tab (detached thread, best-effort).
        if let Some(entry) = group.tabs.get(group.active) {
            let url = entry.last_url.clone().unwrap_or_else(|| {
                store
                    .get(&entry.app_id)
                    .map(|a| a.url)
                    .unwrap_or_default()
            });
            if let Ok(u) = url::Url::parse(&url) {
                let app = app.clone();
                let label = group.label.clone();
                std::thread::Builder::new()
                    .name(format!("appmaka-tabbed-tint-{}", group.id))
                    .spawn(move || tint_tabbed_caption(&app, &label, &u))
                    .ok();
            }
        }
    }
    #[cfg(not(windows))]
    {
        if reposition {
            ensure_strip_window(app, group, window);
        } else {
            // Switch path: the strip already exists at the right
            // position; just make sure it is visible.
            set_strip_visible(app, &group.id, true);
        }
        let _ = store;
    }
}

// ---------------------------------------------------------------------------
// Group operations (called from async commands or dedicated threads)
// ---------------------------------------------------------------------------

/// Open a new tabbed window. `initial` tabs are validated against the
/// store; unknown apps/accounts are dropped. `restore_id` reuses a saved
/// group id (session restore keeps pins stable across restarts).
/// Parameters for `open_tabbed_window` (clippy: too_many_arguments).
#[derive(Default)]
pub struct OpenTabbedParams {
    pub initial: Vec<TabEntry>,
    pub active: usize,
    pub placement: Option<WindowPlacement>,
    pub restore_id: Option<String>,
}

pub fn open_tabbed_window(
    app: &AppHandle,
    store: &AppStore,
    adblock: &AdblockState,
    tabstate: &TabState,
    params: OpenTabbedParams,
) -> Result<TabInfo, String> {
    let tabs: Vec<TabEntry> = params
        .initial
        .into_iter()
        .filter(|t| t.valid())
        .collect();
    let active = if tabs.is_empty() {
        0
    } else {
        params.active.min(tabs.len() - 1)
    };
    // Validate against the store so a deleted app never yields a dead tab.
    let mut valid_tabs = Vec::new();
    for t in tabs {
        let ok = store
            .get(&t.app_id)
            .map(|a| a.accounts.iter().any(|ac| ac.id == t.account_id))
            .unwrap_or(false);
        if ok {
            valid_tabs.push(t);
        }
    }
    let id = params.restore_id.unwrap_or_else(new_group_id);
    let label = window_label(&id, 0);
    let entry = valid_tabs.get(active);
    crate::caption::tint_log(app, &format!("tabs: open start group={id} tabs={}", valid_tabs.len()));
    let window = build_tabbed_window(
        app,
        store,
        adblock,
        BuildTabbedArgs {
            label: &label,
            entry,
            group_id: &id,
            placement: params.placement,
            geometry: None,
        },
    )?;
    crate::caption::tint_log(app, &format!("tabs: open built label={label}"));
    let group = TabGroup {
        id: id.clone(),
        label: label.clone(),
        tabs: valid_tabs,
        active,
        generation: 0,
        tint: None,
    };
    attach_strip(app, store, &group, &window, true);
    crate::caption::tint_log(app, &format!("tabs: open strip attached label={label}"));
    let info = tab_info(store, &group, false);
    tabstate
        .inner
        .lock()
        .map_err(|e| format!("tab state lock poisoned: {e}"))?
        .insert(id, group);
    crate::session::write_session(app);
    Ok(info)
}

/// Switch the active tab: hide the old window, rebuild with the new tab's
/// session, close the old window. On build failure the old window is
/// reshown and TabState rolls back, so the user never loses their tab.
///
/// v0.10.1: the TabState mutex is never held across a Tauri window call.
/// Every window getter/setter (`url()`, `outer_position()`, `hide()`…)
/// blocks on the main event loop; holding the mutex across one wedged the
/// whole app on Windows (same family as the v0.10.0 Linux switch hang).
pub fn switch_tab(
    app: &AppHandle,
    store: &AppStore,
    adblock: &AdblockState,
    tabstate: &TabState,
    group_id: &str,
    index: usize,
) -> Result<TabInfo, String> {
    switch_tab_inner(app, store, adblock, tabstate, group_id, index, false)
}

/// Forced switch: rebuild even when `index == group.active`. Used when the
/// first tab is added to an empty group — the live window is still on
/// about:blank, so the normal early-return would leave the tab unloaded
/// and the strip stale.
pub(crate) fn switch_tab_forced(
    app: &AppHandle,
    store: &AppStore,
    adblock: &AdblockState,
    tabstate: &TabState,
    group_id: &str,
    index: usize,
) -> Result<TabInfo, String> {
    switch_tab_inner(app, store, adblock, tabstate, group_id, index, true)
}

fn switch_tab_inner(
    app: &AppHandle,
    store: &AppStore,
    adblock: &AdblockState,
    tabstate: &TabState,
    group_id: &str,
    index: usize,
    force: bool,
) -> Result<TabInfo, String> {
    crate::caption::tint_log(
        app,
        &format!("tabs: switch start group={group_id} index={index} force={force}"),
    );
    // Phase 1: snapshot identity under one short lock. No window calls
    // here — see the doc comment on switch_tab.
    struct Plan {
        old_label: String,
        old_active: usize,
        old_window: Option<WebviewWindow>,
        entry: TabEntry,
        generation: u64,
    }
    let plan = {
        let mut state = tabstate
            .inner
            .lock()
            .map_err(|e| format!("tab state lock poisoned: {e}"))?;
        let group = state
            .get_mut(group_id)
            .ok_or_else(|| "Tabbed window not found.".to_string())?;
        if group.tabs.is_empty() {
            return Err("This tabbed window has no tabs yet — use + to add one.".to_string());
        }
        if index >= group.tabs.len() {
            return Err("Tab index out of range.".to_string());
        }
        if !switch_needs_rebuild(group.active, index, force) {
            let info = tab_info(store, group, false);
            return Ok(info);
        }
        let old_label = group.label.clone();
        let old_active = group.active;
        let old_window = app.get_webview_window(&old_label);
        let entry = group.tabs[index].clone();
        let generation = group.generation + 1;
        // Commit the new identity BEFORE touching windows: any concurrent
        // session write or Destroyed event sees the new label, and the old
        // label's Destroyed handler becomes a no-op by construction.
        group.active = index;
        group.generation = generation;
        group.label = window_label(group_id, generation);
        group.tint = None;
        Plan {
            old_label,
            old_active,
            old_window,
            entry,
            generation,
        }
    };

    // Phase 2: query the old window with NO lock held. url() and the
    // geometry getters all block on the main event loop; the mutex stays
    // free so a slow main thread can never wedge the app.
    let (last_url, geometry) = match plan.old_window.as_ref() {
        Some(w) => {
            let url = w
                .url()
                .ok()
                .map(|u| u.to_string())
                .filter(|s| s != "about:blank");
            (url, save_geometry(w))
        }
        None => (None, None),
    };
    // Phase 3: remember where the old tab was. Best-effort: only if the
    // group is still at the generation we committed.
    if let Some(u) = last_url {
        if let Ok(mut state) = tabstate.inner.lock() {
            if let Some(group) = state.get_mut(group_id) {
                if group.generation == plan.generation {
                    if let Some(e) = group.tabs.get_mut(plan.old_active) {
                        e.last_url = Some(u);
                    }
                }
            }
        }
    }
    crate::caption::tint_log(
        app,
        &format!("tabs: switch queries done group={group_id} gen={}", plan.generation),
    );

    if let Some(w) = plan.old_window.as_ref() {
        let _ = w.hide();
    }
    let new_label = window_label(group_id, plan.generation);
    let build = build_tabbed_window(
        app,
        store,
        adblock,
        BuildTabbedArgs {
            label: &new_label,
            entry: Some(&plan.entry),
            group_id,
            placement: None,
            geometry,
        },
    );
    match build {
        Ok(window) => {
            if let Some(w) = plan.old_window.as_ref() {
                let _ = w.close();
            }
            // v0.11.2: the old generation's strip must die with the
            // switch, unconditionally. The caption HWND lives on the
            // chrome thread and is NOT torn down by w.close(); the
            // Destroyed handler can't resolve the old label either (the
            // group already carries the new one), so skipping this call
            // strands an orphaned, unclosable strip on the desktop.
            // Idempotent: a second call is a no-op.
            #[cfg(windows)]
            crate::caption::page_window_closed(&plan.old_label);
            // Snapshot the strip data under a short lock, then attach with
            // the lock released: attach_strip queries scale_factor(),
            // which blocks on the main event loop (same hang family).
            let snapshot_group = {
                let state = tabstate
                    .inner
                    .lock()
                    .map_err(|e| format!("tab state lock poisoned: {e}"))?;
                state.get(group_id).cloned()
            };
            if let Some(g) = snapshot_group.as_ref() {
                attach_strip(app, store, g, &window, false);
            }
            let _ = window.set_focus();
            let info = {
                let state = tabstate
                    .inner
                    .lock()
                    .map_err(|e| format!("tab state lock poisoned: {e}"))?;
                let group = state
                    .get(group_id)
                    .ok_or_else(|| "Tabbed window was closed during the switch.".to_string())?;
                tab_info(store, group, false)
            };
            crate::caption::tint_log(
                app,
                &format!("tabs: switch done group={group_id} label={new_label}"),
            );
            crate::session::write_session(app);
            Ok(info)
        }
        Err(e) => {
            // Roll back: reshow the old window, restore the old identity.
            if let Some(w) = plan.old_window.as_ref() {
                let _ = w.show();
                let _ = w.set_focus();
            }
            if let Ok(mut state) = tabstate.inner.lock() {
                if let Some(group) = state.get_mut(group_id) {
                    group.generation = plan.generation - 1;
                    group.label = plan.old_label;
                    group.active = plan.old_active;
                }
            }
            Err(format!("Couldn't switch tabs ({e}). Kept the current tab."))
        }
    }
}

/// Add a tab to a group and switch to it. Validates the app/account
/// against the store first.
pub fn add_tab(
    app: &AppHandle,
    store: &AppStore,
    adblock: &AdblockState,
    tabstate: &TabState,
    group_id: &str,
    app_id: &str,
    account_id: &str,
) -> Result<TabInfo, String> {
    let web_app = store.get(app_id)?;
    if !web_app.accounts.iter().any(|a| a.id == account_id) {
        return Err("Account not found.".to_string());
    }
    // Adding a tab that's already there just activates it.
    let existing = {
        let state = tabstate
            .inner
            .lock()
            .map_err(|e| format!("tab state lock poisoned: {e}"))?;
        let group = state
            .get(group_id)
            .ok_or_else(|| "Tabbed window not found.".to_string())?;
        group
            .tabs
            .iter()
            .position(|t| t.app_id == app_id && t.account_id == account_id)
    };
    if let Some(i) = existing {
        return switch_tab(app, store, adblock, tabstate, group_id, i);
    }
    let (index, was_empty) = {
        let mut state = tabstate
            .inner
            .lock()
            .map_err(|e| format!("tab state lock poisoned: {e}"))?;
        let group = state
            .get_mut(group_id)
            .ok_or_else(|| "Tabbed window not found.".to_string())?;
        let was_empty = group.tabs.is_empty();
        group.tabs.push(TabEntry {
            app_id: app_id.to_string(),
            account_id: account_id.to_string(),
            last_url: None,
        });
        (group.tabs.len() - 1, was_empty)
    };
    // First tab in an empty group: the live window is still on about:blank,
    // so a normal switch would early-return (index == active) and leave the
    // tab unloaded with a stale strip. Force the rebuild.
    let info = if was_empty {
        switch_tab_forced(app, store, adblock, tabstate, group_id, index)?
    } else {
        switch_tab(app, store, adblock, tabstate, group_id, index)?
    };
    crate::session::write_session(app);
    Ok(info)
}

/// Close one tab. Closing the last tab closes the whole group (browser
/// behavior). Returns the updated info, or `group_closed: true`.
pub fn close_tab(
    app: &AppHandle,
    store: &AppStore,
    adblock: &AdblockState,
    tabstate: &TabState,
    group_id: &str,
    index: usize,
) -> Result<TabInfo, String> {
    let (should_close_group, new_active) = {
        let mut state = tabstate
            .inner
            .lock()
            .map_err(|e| format!("tab state lock poisoned: {e}"))?;
        let group = state
            .get_mut(group_id)
            .ok_or_else(|| "Tabbed window not found.".to_string())?;
        if index >= group.tabs.len() {
            return Err("Tab index out of range.".to_string());
        }
        group.tabs.remove(index);
        if group.tabs.is_empty() {
            (true, 0)
        } else {
            let new_active = if index == group.active {
                index.min(group.tabs.len() - 1)
            } else if index < group.active {
                group.active - 1
            } else {
                group.active
            };
            group.active = new_active;
            (false, new_active)
        }
    };
    if should_close_group {
        close_group(app, tabstate, group_id);
        // Return a tombstone info so the strip can clear itself.
        return Ok(TabInfo {
            group_id: group_id.to_string(),
            tabs: Vec::new(),
            active: 0,
            tint: None,
            group_closed: true,
        });
    }
    // If the closed tab was active (or before it), the live window shows
    // the wrong tab — rebuild on the new active tab. Otherwise just
    // refresh the strip.
    let needs_rebuild = {
        let state = tabstate
            .inner
            .lock()
            .map_err(|e| format!("tab state lock poisoned: {e}"))?;
        // Rebuild whenever the removed tab was at or before the active
        // one: the live webview no longer matches the active tab.
        state.get(group_id).is_some() && index <= new_active
    };
    if needs_rebuild {
        let info = switch_tab(app, store, adblock, tabstate, group_id, new_active)?;
        crate::session::write_session(app);
        Ok(info)
    } else {
        let (info, label) = {
            let state = tabstate
                .inner
                .lock()
                .map_err(|e| format!("tab state lock poisoned: {e}"))?;
            let group = state
                .get(group_id)
                .ok_or_else(|| "Tabbed window not found.".to_string())?;
            (tab_info(store, group, false), group.label.clone())
        };
        refresh_strip(app, store, tabstate, group_id, &label);
        crate::session::write_session(app);
        Ok(info)
    }
}

/// Close the whole group: remove state, close the page window (its
/// Destroyed handler no-ops — the group is already gone), close the Linux
/// strip, drop the caption strip, and persist.
pub fn close_group(app: &AppHandle, tabstate: &TabState, group_id: &str) {
    let label = {
        match tabstate.inner.lock() {
            Ok(mut state) => state.remove(group_id).map(|g| g.label),
            Err(_) => None,
        }
    };
    if let Some(label) = label {
        #[cfg(windows)]
        crate::caption::page_window_closed(&label);
        #[cfg(not(windows))]
        close_strip_window(app, group_id);
        if let Some(w) = app.get_webview_window(&label) {
            let _ = w.close();
        }
        // Best-effort cleanup of the empty-group profile dir.
        if let Ok(dir) = app.path().app_data_dir() {
            let _ = std::fs::remove_dir_all(dir.join("tabbed-profiles").join(group_id));
        }
    }
    crate::session::write_session(app);
}

/// User-intent close of a whole tabbed window (dashboard row, strip X,
/// Esc+LMB gesture): pinned groups get the one native confirm, exactly
/// like pinned account windows.
pub fn close_tabbed_window(app: &AppHandle, tabstate: &TabState, group_id: &str) -> Result<(), String> {
    if !crate::pin::guard_close(app, &pin_key(group_id)) {
        return Ok(());
    }
    close_group(app, tabstate, group_id);
    Ok(())
}

/// Extract the group id from a generation-suffixed tabbed-window label
/// (`tabbed-{group_id}-g{generation}`). Pure logic, unit-tested.
#[cfg(windows)]
fn group_id_from_label(label: &str) -> Option<&str> {
    let rest = label.strip_prefix("tabbed-")?;
    let (gid, gen) = rest.rsplit_once("-g")?;
    gen.parse::<u64>().ok()?;
    Some(gid)
}

/// Resolve a live or prior-generation tabbed-window label to its group
/// id. Exact label match first; falls back to parsing the group id out
/// of the label so clicks on an in-flight older strip still reach the
/// live group instead of silently doing nothing.
#[cfg(windows)]
pub fn group_id_for_label(app: &AppHandle, label: &str) -> Option<String> {
    // Bind the state guard: try_state returns a temporary whose borrow
    // must outlive the lock guard (E0716 otherwise).
    let tab_state = app.try_state::<TabState>()?;
    let state = tab_state.inner.lock().ok()?;
    if let Some((id, _)) = state.iter().find(|(_, g)| g.label == label) {
        return Some(id.clone());
    }
    if let Some(gid) = group_id_from_label(label) {
        if state.contains_key(gid) {
            return Some(gid.to_string());
        }
    }
    None
}

/// Fire-and-forget close of a whole tabbed window by live label (strip X
/// button, dashboard-adjacent paths): resolves the group and runs the
/// pin-aware user-intent close on a worker thread. Safe to call from the
/// chrome thread or the strip proc — the pin confirm blocks the worker,
/// never the caller.
#[cfg(windows)]
pub fn request_close_group_by_label(app: &AppHandle, label: &str) {
    let Some(group_id) = group_id_for_label(app, label) else {
        return;
    };
    let app = app.clone();
    std::thread::Builder::new()
        .name("appmaka-tab-close".to_string())
        .spawn(move || {
            if let Some(ts) = app.try_state::<TabState>() {
                let _ = close_tabbed_window(&app, &ts, &group_id);
            }
        })
        .ok();
}

/// Esc+LMB gesture inside a tabbed window (v0.10.0): closes the ACTIVE
/// TAB, not the window — matching the gesture's "close what I'm pointing
/// at" feel. When it's the last tab, the window itself closes, pin-aware
/// like the X button. (Closing a tab in a pinned group needs no confirm:
/// the pin protects the window, and the window survives.)
///
/// Worker-thread entry: the last-tab path may block on the pin confirm.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub fn gesture_close_active_tab(app: &AppHandle, label: &str) {
    let Some(tabstate) = app.try_state::<TabState>() else {
        return;
    };
    let (group_id, active, n_tabs) = match tabstate.inner.lock() {
        Ok(s) => match s.iter().find(|(_, g)| g.label == label) {
            Some((id, g)) => (id.clone(), g.active, g.tabs.len()),
            None => return,
        },
        Err(_) => return,
    };
    if n_tabs > 1 {
        let (Some(store), Some(adblock)) = (
            app.try_state::<AppStore>(),
            app.try_state::<AdblockState>(),
        ) else {
            return;
        };
        let _ = close_tab(app, &store, &adblock, &tabstate, &group_id, active);
    } else {
        // Last tab: this closes the window — pin-aware user-intent close.
        let _ = close_tabbed_window(app, &tabstate, &group_id);
    }
}

/// The X button / gesture closed the page window directly: drop the group
/// (it no longer has a window), close the Linux strip, persist. Called
/// from the page window's Destroyed handler.
///
/// v0.11.2: the strip for `label` is dropped unconditionally at entry. A
/// destroyed window must never keep its caption: the strip HWND lives on
/// the chrome thread and outlives cross-thread owner teardown, and during
/// a tab switch the group already carries the NEW label so the old label
/// resolves to nothing below. page_window_closed is a non-blocking
/// channel send and idempotent, so this is safe on the main thread.
pub fn on_tabbed_window_destroyed(app: &AppHandle, label: &str) {
    #[cfg(windows)]
    crate::caption::page_window_closed(label);
    // v0.10.1: this runs on the main thread (WindowEvent::Destroyed). It
    // must NEVER block on the TabState lock: if a worker holds the lock
    // while waiting on a main-thread Tauri call (e.g. scale_factor), a
    // blocking lock() here deadlocks the app ("Not Responding", 0% CPU).
    // Try the lock; if a worker holds it, defer the removal to a worker
    // thread that may block safely.
    fn remove_group(app: &AppHandle, label: &str) -> Option<String> {
        let ts = app.try_state::<TabState>()?;
        let mut state = ts.inner.try_lock().ok()?;
        let id = state
            .iter()
            .find(|(_, g)| g.label == label)
            .map(|(id, _)| id.clone());
        if let Some(ref id) = id {
            state.remove(id);
        }
        id
    }
    fn finish_cleanup(app: &AppHandle, label: &str, id: &str) {
        #[cfg(windows)]
        {
            crate::caption::page_window_closed(label);
            let _ = id;
        }
        #[cfg(not(windows))]
        {
            let _ = label;
            close_strip_window(app, id);
        }
        crate::session::write_session(app);
    }

    if let Some(id) = remove_group(app, label) {
        finish_cleanup(app, label, &id);
    } else if app.try_state::<TabState>().is_some() {
        // The lock is held by a worker: do the removal off the main thread.
        let app = app.clone();
        let label = label.to_string();
        std::thread::Builder::new()
            .name("appmaka-tab-destroy-cleanup".to_string())
            .spawn(move || {
                // Blocking lock is safe here: this is not the main thread,
                // so it cannot deadlock against a main-thread Tauri call.
                let id = app.try_state::<TabState>().and_then(|ts| {
                    ts.inner
                        .lock()
                        .ok()
                        .and_then(|mut state| {
                            let id = state
                                .iter()
                                .find(|(_, g)| g.label == label)
                                .map(|(id, _)| id.clone());
                            if let Some(ref id) = id {
                                state.remove(id);
                            }
                            id
                        })
                });
                if let Some(id) = id {
                    finish_cleanup(&app, &label, &id);
                }
            })
            .ok();
    }
}

/// Live groups for session writes and the dashboard.
pub fn live_tab_groups(app: &AppHandle) -> Vec<LiveTabGroup> {
    let store = match app.try_state::<AppStore>() {
        Some(s) => s,
        None => return Vec::new(),
    };
    match app.try_state::<TabState>() {
        Some(ts) => match ts.inner.lock() {
            Ok(state) => {
                let snapshots: Vec<TabGroup> = state.values().cloned().collect();
                snapshots
                    .into_iter()
                    .filter(|g| app.get_webview_window(&g.label).is_some())
                    .map(|g| LiveTabGroup {
                        id: g.id.clone(),
                        tabs: g.tabs.clone(),
                        active: g.active,
                        label: g.label.clone(),
                        pinned: crate::pin::is_pinned(app, &pin_key(&g.id)),
                        tab_names: g.tabs.iter().map(|t| tab_summary(&store, t)).collect(),
                    })
                    .collect()
            }
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    }
}

/// Session/restore/dashboard view of one live group.
#[derive(Debug, Clone)]
pub struct LiveTabGroup {
    pub id: String,
    pub tabs: Vec<TabEntry>,
    pub active: usize,
    pub label: String,
    pub pinned: bool,
    pub tab_names: Vec<TabSummary>,
}

/// Resolve a keyboard tab-switch (Ctrl+Tab etc.) for the group whose live
/// window has `label`. Returns (group id, target index).
#[cfg(windows)]
pub fn resolve_key_switch(
    tabstate: &TabState,
    label: &str,
    action: TabKeyAction,
) -> Option<(String, usize)> {
    let s = tabstate.inner.lock().ok()?;
    let (id, g) = s.iter().find(|(_, g)| g.label == label)?;
    let idx = resolve_key_action(action, g.active, g.tabs.len())?;
    Some((id.clone(), idx))
}

/// A clone of one live group (for strip refreshes without holding the lock).
#[cfg(windows)]
pub fn tab_group(tabstate: &TabState, group_id: &str) -> Option<TabGroup> {
    tabstate.inner.lock().ok()?.get(group_id).cloned()
}

/// Group ids + pin keys for the RAM dashboard's close-all sweep.
pub fn all_groups_for_close_all(tabstate: &TabState) -> Vec<(String, String)> {
    match tabstate.inner.lock() {
        Ok(s) => s.keys().map(|id| (id.clone(), pin_key(id))).collect(),
        Err(_) => Vec::new(),
    }
}

/// One tabbed window's restore spec: (group id, tabs, active index, last
/// rect, last generation). Aliased for clippy::type_complexity.
pub type TabbedRestoreSpec = (String, Vec<TabEntry>, usize, Option<WindowRect>, u64);

/// Dashboard rows: one per open tabbed window.
pub fn list_tabbed_windows(app: &AppHandle) -> Vec<TabbedWindowInfo> {
    live_tab_groups(app)
        .into_iter()
        .map(|g| {
            let focused = app
                .get_webview_window(&g.label)
                .and_then(|w| w.is_focused().ok())
                .unwrap_or(false);
            let tint = app
                .try_state::<TabState>()
                .and_then(|ts| {
                    ts.inner.lock().ok().and_then(|s| {
                        s.get(&g.id).and_then(|gr| gr.tint.clone())
                    })
                });
            TabbedWindowInfo {
                group_id: g.id,
                label: g.label,
                tabs: g.tab_names,
                active: g.active,
                tint,
                focused,
                pinned: g.pinned,
            }
        })
        .collect()
}

/// Rebuild the tab-strip snapshot for a group (Windows) or re-render the
/// HTML strip (Linux) after a metadata-only change (tab closed without a
/// rebuild, tab added while inactive, etc.).
fn refresh_strip(
    app: &AppHandle,
    store: &AppStore,
    tabstate: &TabState,
    group_id: &str,
    label: &str,
) {
    #[cfg(windows)]
    {
        let _ = app;
        if let Some(g) = tab_group(tabstate, group_id) {
            let snapshot = tab_strip_snapshot(store, &g);
            crate::caption::tabbed_window_updated(label, snapshot);
        }
    }
    #[cfg(not(windows))]
    {
        // The Linux strip re-renders from list_tabbed_windows on every
        // command response; just nudge it in case this path wasn't
        // command-driven.
        if let Some(strip) = app.get_webview_window(&strip_label(group_id)) {
            let _ = strip.eval("window.__refreshTabs && window.__refreshTabs()");
        }
        let _ = (store, tabstate, label);
    }
}

// ---------------------------------------------------------------------------
// Title-bar blending: active tab's theme-color -> strip background
// ---------------------------------------------------------------------------

/// Detached-thread entry: tint the strip with the active tab's color.
/// Called on tab switch and on navigation.
///
/// The group state's hex (fast HTTP path, synchronous) feeds the Linux
/// HTML strip and the dashboard via the list response; on Windows the
/// native strip additionally follows the full universal chain
/// (theme-color → live-DOM probe → default) through `tint::request_retint`
/// (v0.11.0).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn tint_tabbed_caption(app: &AppHandle, label: &str, url: &url::Url) {
    let url_str = url.to_string();
    let decision = crate::tint::resolve_fast(&url_str);
    set_group_tint(
        app,
        label,
        decision
            .rgb
            .map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}")),
    );
    crate::tint::request_retint(app, label, url, crate::tint::TintTarget::TabStrip);
}

#[cfg_attr(not(windows), allow(dead_code))]
fn set_group_tint(app: &AppHandle, label: &str, tint: Option<String>) {
    if let Some(ts) = app.try_state::<TabState>() {
        if let Ok(mut state) = ts.inner.lock() {
            if let Some(group) = state.values_mut().find(|g| g.label == label) {
                group.tint = tint;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot for the Windows native strip
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn tab_display_name(store: &AppStore, entry: &TabEntry) -> String {
    match store.get(&entry.app_id) {
        Ok(web_app) => {
            let label = web_app
                .accounts
                .iter()
                .find(|a| a.id == entry.account_id)
                .map(|a| a.label.clone())
                .unwrap_or_default();
            // Disambiguate multi-account apps: "Gmail — Work".
            if web_app.accounts.len() > 1 && !label.is_empty() {
                format!("{} — {}", web_app.name, label)
            } else {
                web_app.name
            }
        }
        Err(_) => "Unknown app".to_string(),
    }
}

/// Tab names + active index + tint for caption.rs painting.
#[cfg(windows)]
fn tab_strip_snapshot(store: &AppStore, group: &TabGroup) -> crate::caption::TabStripData {
    crate::caption::TabStripData {
        tabs: group
            .tabs
            .iter()
            .map(|t| tab_display_name(store, t))
            .collect(),
        active: group.active,
        tint: group
            .tint
            .as_deref()
            .and_then(crate::tint::parse_css_color),
    }
}

// ---------------------------------------------------------------------------
// Linux HTML tab strip window
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
fn strip_label(group_id: &str) -> String {
    format!("tabstrip-{group_id}")
}

#[cfg(not(windows))]
const STRIP_H: f64 = 44.0;

/// Build (or reposition) the HTML tab strip above the page window. The
/// strip is our own trusted UI (IPC on); the page keeps withGlobalTauri
/// off exactly like account windows.
#[cfg(not(windows))]
fn ensure_strip_window(app: &AppHandle, group: &TabGroup, page: &WebviewWindow) {
    use tauri::{Position, Size};
    let label = strip_label(&group.id);
    let scale = page.scale_factor().unwrap_or(1.0).max(0.1);
    let (px, py, pw) = match (page.outer_position(), page.outer_size()) {
        (Ok(p), Ok(s)) => (p.x as f64 / scale, p.y as f64 / scale, s.width as f64 / scale),
        _ => return,
    };
    let sx = px;
    let sy = (py - STRIP_H).max(0.0);
    if let Some(strip) = app.get_webview_window(&label) {
        let _ = strip.set_position(Position::Logical(tauri::LogicalPosition::new(sx, sy)));
        let _ = strip.set_size(Size::Logical(tauri::LogicalSize::new(pw, STRIP_H)));
        return;
    }
    let strip = match WebviewWindowBuilder::new(
        app,
        &label,
        WebviewUrl::App("tabstrip.html".into()),
    )
    .title("AppMaka tabs")
    .decorations(false)
    .skip_taskbar(true)
    .inner_size(pw, STRIP_H)
    .position(sx, sy)
    .build()
    {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[appmaka] tab strip window failed for group {}: {e}", group.id);
            return;
        }
    };
    // The strip is not independently closable: Alt+F4 (or the X) on the
    // strip closes the whole tabbed window instead.
    let s_app = app.clone();
    let s_group = group.id.clone();
    strip.on_window_event(move |event| {
        if matches!(event, WindowEvent::CloseRequested { .. }) {
            if let Some(ts) = s_app.try_state::<TabState>() {
                let _ = close_tabbed_window(&s_app, &ts, &s_group);
            }
        }
    });
}

#[cfg(not(windows))]
fn reposition_strip_window(app: &AppHandle, group_id: &str) {
    use tauri::{Position, Size};
    let (page, strip) = match app.try_state::<TabState>().and_then(|ts| {
        ts.inner.lock().ok().and_then(|s| {
            s.get(group_id).map(|g| {
                (
                    app.get_webview_window(&g.label),
                    app.get_webview_window(&strip_label(group_id)),
                )
            })
        })
    }) {
        Some((p, s)) => (p, s),
        None => return,
    };
    let (Some(page), Some(strip)) = (page, strip) else {
        return;
    };
    let scale = page.scale_factor().unwrap_or(1.0).max(0.1);
    if let (Ok(p), Ok(s)) = (page.outer_position(), page.outer_size()) {
        let sx = p.x as f64 / scale;
        let sy = (p.y as f64 / scale - STRIP_H).max(0.0);
        let pw = s.width as f64 / scale;
        let _ = strip.set_position(Position::Logical(tauri::LogicalPosition::new(sx, sy)));
        let _ = strip.set_size(Size::Logical(tauri::LogicalSize::new(pw, STRIP_H)));
    }
}

#[cfg(not(windows))]
fn set_strip_visible(app: &AppHandle, group_id: &str, visible: bool) {
    if let Some(strip) = app.get_webview_window(&strip_label(group_id)) {
        if visible {
            let _ = strip.show();
        } else {
            let _ = strip.hide();
        }
    }
}

#[cfg(not(windows))]
fn close_strip_window(app: &AppHandle, group_id: &str) {
    if let Some(strip) = app.get_webview_window(&strip_label(group_id)) {
        let _ = strip.close();
    }
}

// ---------------------------------------------------------------------------
// Unit tests: pure logic only (no windows built)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use layout::*;

    #[test]
    fn tab_key_classifier() {
        // Ctrl+Tab -> next, Ctrl+Shift+Tab -> prev.
        assert_eq!(
            tab_key_action(0x09, true, false),
            Some(TabKeyAction::Next)
        );
        assert_eq!(
            tab_key_action(0x09, true, true),
            Some(TabKeyAction::Prev)
        );
        // Ctrl+3 -> third tab.
        assert_eq!(
            tab_key_action(0x33, true, false),
            Some(TabKeyAction::Index(2))
        );
        // No Ctrl: everything passes through.
        assert_eq!(tab_key_action(0x09, false, false), None);
        assert_eq!(tab_key_action(0x33, false, false), None);
        // Other keys with Ctrl: untouched.
        assert_eq!(tab_key_action(0x41, true, false), None);
    }

    #[test]
    fn wrap_index_wraps_both_ways() {
        assert_eq!(wrap_index(0, 3, 1), 1);
        assert_eq!(wrap_index(2, 3, 1), 0);
        assert_eq!(wrap_index(0, 3, -1), 2);
        assert_eq!(wrap_index(1, 3, -1), 0);
        assert_eq!(wrap_index(0, 1, 1), 0);
        assert_eq!(wrap_index(0, 0, 1), 0);
    }

    #[test]
    fn resolve_key_action_noops() {
        // Next on the last tab wraps to the first.
        assert_eq!(
            resolve_key_action(TabKeyAction::Next, 2, 3),
            Some(0)
        );
        // Single tab: Next/Prev are no-ops.
        assert_eq!(resolve_key_action(TabKeyAction::Next, 0, 1), None);
        assert_eq!(resolve_key_action(TabKeyAction::Prev, 0, 1), None);
        // Number key out of range: no-op.
        assert_eq!(resolve_key_action(TabKeyAction::Index(5), 0, 3), None);
        // Number key on the active tab: no-op.
        assert_eq!(resolve_key_action(TabKeyAction::Index(1), 1, 3), None);
        assert_eq!(resolve_key_action(TabKeyAction::Index(0), 1, 3), Some(0));
    }

    #[test]
    fn tint_contrast_boundary() {
        // Dark tints want light text; light tints want dark text.
        assert!(tint_wants_light_text(27, 27, 27));
        assert!(tint_wants_light_text(120, 20, 20));
        assert!(!tint_wants_light_text(255, 255, 255));
        assert!(!tint_wants_light_text(200, 200, 200));
    }

    #[test]
    fn tab_ranges_share_space_evenly() {
        let (ranges, add_x) = tab_ranges(1200.0, 3);
        assert_eq!(ranges.len(), 3);
        // free = 1200 - 138 - 34 = 1028; 1028/3 = 342.67 -> clamped to 168.
        for (x0, x1) in &ranges {
            assert!((x1 - x0 - TAB_MAX_W).abs() < 1e-6);
        }
        assert!((add_x - 3.0 * TAB_MAX_W).abs() < 1e-6);
    }

    #[test]
    fn tab_ranges_shrink_to_minimum() {
        // Narrow strip: 300 - 138 - 34 = 128 free for 4 tabs -> 32 each,
        // clamped up to TAB_MIN_W (overflow is honest, not silent).
        let (ranges, _) = tab_ranges(300.0, 4);
        assert_eq!(ranges.len(), 4);
        for (x0, x1) in &ranges {
            assert!((x1 - x0 - TAB_MIN_W).abs() < 1e-6);
        }
    }

    #[test]
    fn tab_ranges_empty_group() {
        let (ranges, add_x) = tab_ranges(1200.0, 0);
        assert!(ranges.is_empty());
        assert_eq!(add_x, 0.0);
    }

    #[test]
    fn hit_test_tabs_close_add_and_buttons() {
        let strip_w = 1200.0;
        let close_w = 20.0;
        // First tab body.
        assert_eq!(
            tab_hit_test(10.0, 10.0, strip_w, 2, close_w),
            TabHit::Tab(0)
        );
        // First tab's close affordance (right 20px of the 168px tab).
        assert_eq!(
            tab_hit_test(160.0, 10.0, strip_w, 2, close_w),
            TabHit::CloseTab(0)
        );
        // Second tab body.
        assert_eq!(
            tab_hit_test(200.0, 10.0, strip_w, 2, close_w),
            TabHit::Tab(1)
        );
        // "+" right after the last tab (2*168=336).
        assert_eq!(
            tab_hit_test(340.0, 10.0, strip_w, 2, close_w),
            TabHit::Add
        );
        // Gap between + and the window buttons: drag region.
        assert_eq!(
            tab_hit_test(600.0, 10.0, strip_w, 2, close_w),
            TabHit::Drag
        );
        // Window buttons, rightmost first: min, max, close.
        assert_eq!(
            tab_hit_test(1190.0, 10.0, strip_w, 2, close_w),
            TabHit::Min
        );
        assert_eq!(
            tab_hit_test(1140.0, 10.0, strip_w, 2, close_w),
            TabHit::Max
        );
        assert_eq!(
            tab_hit_test(1090.0, 10.0, strip_w, 2, close_w),
            TabHit::CloseWindow
        );
        // Above/below the strip: not a tab hit (drag passthrough).
        assert_eq!(
            tab_hit_test(10.0, 40.0, strip_w, 2, close_w),
            TabHit::Drag
        );
    }

    #[test]
    fn hit_test_empty_group() {
        // No tabs: + sits at x=0.
        assert_eq!(
            tab_hit_test(10.0, 10.0, 1200.0, 0, 20.0),
            TabHit::Add
        );
    }

    #[test]
    fn tab_entry_serde_defaults() {
        // Legacy hand-written entries without new fields still parse.
        let e: TabEntry = serde_json::from_str(
            r#"{"appId": "a1", "accountId": "c1"}"#,
        )
        .unwrap();
        assert_eq!(e.app_id, "a1");
        assert_eq!(e.account_id, "c1");
        assert_eq!(e.last_url, None);
        assert!(e.valid());
        let bad: TabEntry = serde_json::from_str(
            r#"{"appId": "", "accountId": "c1"}"#,
        )
        .unwrap();
        assert!(!bad.valid());
    }

    #[test]
    fn pin_key_stable_across_generations() {
        // The pin key must not depend on the generation-suffixed label.
        assert_eq!(pin_key("g1-2"), "tabbed:g1-2");
        assert!(is_tabbed_label("tabbed-g1-2-g7"));
        assert!(!is_tabbed_label("acct-a-b"));
    }

    #[test]
    fn rebuild_decision() {
        // Same tab, no force: nothing to do (the live window already
        // shows it).
        assert!(!switch_needs_rebuild(0, 0, false));
        assert!(!switch_needs_rebuild(2, 2, false));
        // Different tab: rebuild.
        assert!(switch_needs_rebuild(0, 1, false));
        assert!(switch_needs_rebuild(1, 0, false));
        // First tab in an empty group: the live window is still on
        // about:blank, so force rebuilds even for the active index.
        // (v0.10.0 returned early here and the tab never loaded.)
        assert!(switch_needs_rebuild(0, 0, true));
        assert!(switch_needs_rebuild(2, 2, true));
    }

    #[test]
    #[cfg(windows)]
    fn group_id_from_generation_label() {
        // Current and older generations resolve to the group id.
        assert_eq!(group_id_from_label("tabbed-g25932-1-g0"), Some("g25932-1"));
        assert_eq!(group_id_from_label("tabbed-g25932-1-g6"), Some("g25932-1"));
        // Not a tabbed label, or no numeric generation: no match.
        assert_eq!(group_id_from_label("page-acct-1"), None);
        assert_eq!(group_id_from_label("tabbed-g25932-1"), None);
        assert_eq!(group_id_from_label("tabbed-g25932-1-gx"), None);
    }
}
