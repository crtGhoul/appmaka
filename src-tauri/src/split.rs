//! Split view (v0.18.8): up to four apps side by side in ONE window.
//!
//! A split is one frame window holding child webviews: the bar is a child
//! webview across the top, each pane is a child webview below it, all
//! added with Tauri's multi-webview API (`Window::add_child`, which
//! needs the `unstable` cargo feature — enabled for this in Cargo.toml
//! since v0.18.8; the runtime side of that feature is empty, it only
//! gates Tauri's own unstable APIs). v0.18.7 tiled ordinary account
//! windows under a shared bar window, but the user asked for a real
//! single window — moving the frame moves every pane with it, one
//! taskbar entry, one Alt+Tab entry — so the panes became children of
//! the frame instead.
//!
//! Each pane keeps its own session dir (the same per-account dir
//! account windows use), the same adblock scripts, cursor, popup
//! policy and downloads — the pane builder in windows.rs is the
//! account-window builder with a different host. Panes have no caption
//! strip: the frame's bar is the only chrome, with pane chips (click
//! to focus, x to close one pane), layout buttons, and the window
//! controls.
//!
//! Layout is a function of pane count, never a free choice: 2 panes are
//! side-by-side or stacked, 3 panes are one tall + two stacked, 4 panes
//! are a 2x2 grid. Removing a pane reflows the survivors and re-picks
//! the layout; one pane left dissolves the group (the pane reopens as
//! an ordinary window).
//!
//! Saved layouts and open groups persist in `<app-data>/splits.json`
//! (the workspaces.rs pattern: load-or-default, tmp+rename, file lock).
//! Open groups restore at startup after the session restore — panes
//! are never tracked as windows, so the session never sees them.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tauri::{
    window::WindowBuilder, AppHandle, Emitter, Manager, Position, Rect, Size, Webview, WebviewUrl,
    Window, WindowEvent,
};
use tauri::webview::WebviewBuilder;

use crate::pin::CloseIntent;
use crate::store::AppStore;

/// Height of the split bar (logical px).
pub const SPLIT_BAR_H: f64 = 36.0;

/// Maximum panes in one split (the user's ask).
pub const MAX_PANES: usize = 4;

/// Minimum panes: a split of one is just a window.
pub const MIN_PANES: usize = 2;

fn default_frame() -> FrameRect {
    FrameRect {
        x: 0.0,
        y: 0.0,
        w: 1280.0,
        h: 800.0,
    }
}

/// A split's layout. The name is what the frontend shows (serde rename).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitLayout {
    #[serde(rename = "vertical")]
    #[default]
    TwoVertical,
    #[serde(rename = "horizontal")]
    TwoHorizontal,
    #[serde(rename = "three")]
    Three,
    #[serde(rename = "four")]
    Four,
}

/// The layout a pane count implies. Layout is never a free choice — it
/// always matches the count, so removing a pane re-picks it.
pub fn layout_for_count(count: usize) -> Option<SplitLayout> {
    match count {
        2 => Some(SplitLayout::TwoVertical),
        3 => Some(SplitLayout::Three),
        4 => Some(SplitLayout::Four),
        _ => None,
    }
}

/// Layouts a group of this size may switch to (2-pane groups may also
/// stack; larger groups have exactly one layout each).
pub fn layouts_for_count(count: usize) -> Vec<SplitLayout> {
    match count {
        2 => vec![SplitLayout::TwoVertical, SplitLayout::TwoHorizontal],
        3 => vec![SplitLayout::Three],
        4 => vec![SplitLayout::Four],
        _ => vec![],
    }
}

/// One pane: an (app, account) pair. The live child-webview label
/// derives from the group id and the pane's index, never from the
/// account alone — the same account must never share a webview label
/// with a standalone window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SplitPane {
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub account_id: String,
}

impl SplitPane {
    /// The account's standalone-window label (pin state, session dir,
    /// restore seeds are all keyed by it).
    pub fn acct_label(&self) -> String {
        crate::windows::account_window_label(&self.app_id, &self.account_id)
    }
}

/// Prefix for pane child-webview labels. The cursor code consults it
/// (panes are webviews, not windows).
pub const SPLIT_PANE_LABEL_PREFIX: &str = "splitpane-";

/// Frame window label for a group.
pub fn split_frame_label(group_id: &str) -> String {
    format!("split-{group_id}")
}

/// Child-webview label for one pane. Derived from the account, not
/// the pane's position: removing a pane shifts the survivors' indices,
/// and a position-based label would strand them under stale labels.
pub fn pane_webview_label(group_id: &str, app_id: &str, account_id: &str) -> String {
    format!("{SPLIT_PANE_LABEL_PREFIX}{group_id}-{app_id}-{account_id}")
}

/// Logical-pixel rectangle. The group's frame includes the bar at its
/// top; panes fill the area below it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FrameRect {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub w: f64,
    #[serde(default)]
    pub h: f64,
}

impl Default for FrameRect {
    fn default() -> Self {
        default_frame()
    }
}

/// Split a rect into `n` equal columns.
fn split_cols(area: &FrameRect, n: usize) -> Vec<FrameRect> {
    if n == 0 {
        return Vec::new();
    }
    let w = area.w / n as f64;
    (0..n)
        .map(|i| FrameRect {
            x: area.x + i as f64 * w,
            y: area.y,
            w,
            h: area.h,
        })
        .collect()
}

/// Split a rect into `n` equal rows.
fn split_rows(area: &FrameRect, n: usize) -> Vec<FrameRect> {
    if n == 0 {
        return Vec::new();
    }
    let h = area.h / n as f64;
    (0..n)
        .map(|i| FrameRect {
            x: area.x,
            y: area.y + i as f64 * h,
            w: area.w,
            h,
        })
        .collect()
}

/// Pure tiling: pane rects for a frame of this size, relative to the
/// frame's client area (x from 0, y from just below the bar). The rects
/// tile the pane area exactly — no gaps, no overlap.
pub fn pane_rects(layout: SplitLayout, w: f64, h: f64) -> Vec<FrameRect> {
    let area = FrameRect {
        x: 0.0,
        y: SPLIT_BAR_H,
        w,
        h: (h - SPLIT_BAR_H).max(0.0),
    };
    match layout {
        SplitLayout::TwoVertical => split_cols(&area, 2),
        SplitLayout::TwoHorizontal => split_rows(&area, 2),
        SplitLayout::Three => {
            let cols = split_cols(&area, 2);
            let mut out = vec![cols[0]];
            out.extend(split_rows(&cols[1], 2));
            out
        }
        SplitLayout::Four => {
            let rows = split_rows(&area, 2);
            let mut out = Vec::with_capacity(4);
            for r in rows {
                out.extend(split_cols(&r, 2));
            }
            out
        }
    }
}

/// A live split group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitGroup {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub layout: SplitLayout,
    #[serde(default)]
    pub panes: Vec<SplitPane>,
    #[serde(default)]
    pub frame: FrameRect,
    /// Index of the focused pane (the bar's highlighted chip).
    #[serde(default)]
    pub active_pane: usize,
}

/// A saved split layout: a name plus the panes to reopen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSplitLayout {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub layout: SplitLayout,
    #[serde(default)]
    pub panes: Vec<SplitPane>,
}

/// What the frontend shows for one open group: resolved names included.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitInfo {
    pub id: String,
    pub layout: SplitLayout,
    pub panes: Vec<SplitPaneInfo>,
    pub frame: FrameRect,
    pub active_pane: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitPaneInfo {
    pub app_id: String,
    pub account_id: String,
    pub app_name: String,
    pub account_label: String,
}

/// What `list_splits` returns: saved layouts plus open groups.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitList {
    pub layouts: Vec<SavedSplitLayout>,
    pub open: Vec<SplitInfo>,
}

/// Live per-group bookkeeping, managed as Tauri state.
#[derive(Default)]
pub struct SplitState {
    inner: Mutex<HashMap<String, SplitGroup>>,
    /// Per-pane idle bookkeeping (v0.18.10): last gesture/focus time
    /// and whether the watcher shed the pane's memory. Runtime-only,
    /// never persisted — a restart starts every pane fresh.
    pane_activity: Mutex<HashMap<String, PaneActivity>>,
}

impl SplitState {
    /// Live split panes, total (v0.18.10): the window cap counts them.
    pub fn total_panes(&self) -> usize {
        self.inner
            .lock()
            .map(|m| m.values().map(|g| g.panes.len()).sum())
            .unwrap_or(0)
    }

    /// Every live pane's account-window label (v0.18.10): close-all's
    /// one batch confirm covers them.
    pub fn all_pane_labels(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|m| {
                m.values()
                    .flat_map(|g| g.panes.iter().map(|p| p.acct_label()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// (group id, pane count) for every live group (v0.18.10):
    /// close-all closes each group and counts its panes.
    pub fn group_pane_counts(&self) -> Vec<(String, usize)> {
        self.inner
            .lock()
            .map(|m| {
                m.iter()
                    .map(|(id, g)| (id.clone(), g.panes.len()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Last gesture/focus time of a split pane, and whether the idle
/// watcher lowered its memory target (see shed_idle_pane_memory).
#[derive(Default, Clone, Copy)]
struct PaneActivity {
    last_active: u64,
    low_memory: bool,
}

/// Record activity in a split pane (v0.18.10): every gesture inside a
/// pane refreshes its idle clock, and a gesture in a shed pane wakes it
/// (memory target back to NORMAL). No-op for non-pane labels. The COM
/// call runs after the lock is released — never hold a Mutex across a
/// Win32 call.
pub fn touch_pane_activity(app: &AppHandle, webview_label: &str) {
    if !webview_label.starts_with(SPLIT_PANE_LABEL_PREFIX) {
        return;
    }
    let Some(splitstate) = app.try_state::<SplitState>() else {
        return;
    };
    let was_low = {
        let mut activity = match splitstate.pane_activity.lock() {
            Ok(a) => a,
            Err(_) => return,
        };
        let entry = activity.entry(webview_label.to_string()).or_insert(PaneActivity {
            last_active: 0,
            low_memory: false,
        });
        let was_low = entry.low_memory;
        entry.last_active = crate::windows::unix_secs();
        entry.low_memory = false;
        was_low
    };
    if was_low {
        if let Some(w) = app.get_webview(webview_label) {
            crate::windows::set_pane_memory_target_low(&w, false);
        }
        eprintln!("[idle] split pane {webview_label} memory target normal (wake)");
    }
}

/// v0.18.10: idle split panes shed memory like suspended windows. A
/// child webview cannot TrySuspend (no WebviewWindow), so an idle pane
/// gets WebView2's memory target LOW instead: the page stays live
/// (audio keeps playing, scroll and form state intact), Chromium just
/// drops its caches and working set. The frame's active pane and any
/// pane in a focused frame never shed; a pane sheds only after a full
/// idle interval of observed inactivity (first sighting starts the
/// clock). Called from the suspend watcher, once a minute.
pub fn shed_idle_pane_memory(app: &AppHandle, store: &crate::store::AppStore, now: u64) {
    let Some(splitstate) = app.try_state::<SplitState>() else {
        return;
    };
    let groups: Vec<SplitGroup> = match splitstate.inner.lock() {
        Ok(m) => m.values().cloned().collect(),
        Err(_) => return,
    };
    // Decide under the lock; apply outside it (no Win32 call under a
    // Mutex — see AGENTS.md).
    let mut to_shed: Vec<String> = Vec::new();
    for group in &groups {
        if let Some(frame) = app.get_window(&split_frame_label(&group.id)) {
            if frame.is_focused().unwrap_or(false) {
                continue;
            }
        }
        for (i, pane) in group.panes.iter().enumerate() {
            if i == group.active_pane {
                continue;
            }
            let label = pane_webview_label(&group.id, &pane.app_id, &pane.account_id);
            let minutes = match store.get(&pane.app_id) {
                Ok(a) => a.effective_auto_suspend_minutes(&pane.account_id),
                Err(_) => continue,
            };
            if minutes == 0 {
                continue;
            }
            let mut activity = match splitstate.pane_activity.lock() {
                Ok(a) => a,
                Err(_) => continue,
            };
            match activity.get_mut(&label) {
                None => {
                    activity.insert(
                        label,
                        PaneActivity {
                            last_active: now,
                            low_memory: false,
                        },
                    );
                }
                Some(a) if a.low_memory => {}
                Some(a) => {
                    if now.saturating_sub(a.last_active) >= minutes * 60 {
                        to_shed.push(label);
                    }
                }
            }
        }
    }
    for label in to_shed {
        if let Some(w) = app.get_webview(&label) {
            crate::windows::set_pane_memory_target_low(&w, true);
        }
        if let Ok(mut activity) = splitstate.pane_activity.lock() {
            if let Some(a) = activity.get_mut(&label) {
                a.low_memory = true;
            }
        }
        eprintln!("[idle] split pane {label} memory target low (idle)");
    }
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Serializes read-modify-write cycles so two commands can't interleave saves.
static FILE_LOCK: Mutex<()> = Mutex::new(());

fn new_id() -> String {
    let n = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "sp-{}-{n}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    )
}

/// On-disk shape of splits.json.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SplitFile {
    #[serde(default)]
    layouts: Vec<SavedSplitLayout>,
    #[serde(default)]
    open: Vec<SplitGroup>,
}

fn file_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("splits.json"))
}

/// Load-or-default: a missing or corrupt file is just "no splits".
fn load_file(app: &AppHandle) -> SplitFile {
    file_path(app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|c| serde_json::from_str::<SplitFile>(&c).ok())
        .map(|f| SplitFile {
            layouts: f
                .layouts
                .into_iter()
                .filter(|l| !l.id.is_empty() && !l.name.trim().is_empty())
                .collect(),
            open: f
                .open
                .into_iter()
                .filter(|g| !g.id.is_empty() && g.panes.len() >= MIN_PANES)
                .collect(),
        })
        .unwrap_or_default()
}

fn save_file(app: &AppHandle, file: &SplitFile) -> Result<(), String> {
    let path = file_path(app)?;
    let json =
        serde_json::to_string_pretty(file).map_err(|e| format!("could not encode: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| format!("could not write: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("could not save: {e}"))?;
    Ok(())
}

fn lock_file() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    FILE_LOCK.lock().map_err(|_| "split store busy".to_string())
}

/// Persist the open groups alongside the saved layouts. Callers hold no
/// state lock here — the file is the source of truth for restarts.
fn persist_open(app: &AppHandle, splitstate: &SplitState) -> Result<(), String> {
    let groups: Vec<SplitGroup> = splitstate
        .inner
        .lock()
        .map_err(|_| "split state lock poisoned".to_string())?
        .values()
        .cloned()
        .collect();
    let _guard = lock_file()?;
    let mut file = load_file(app);
    file.open = groups;
    save_file(app, &file)
}

fn emit_changed(app: &AppHandle) {
    if let Ok(list) = list_splits_inner(app) {
        let _ = app.emit("appmaka:split-changed", list);
    }
}

fn info_for(store: &AppStore, group: &SplitGroup) -> SplitInfo {
    let panes = group
        .panes
        .iter()
        .map(|p| {
            let (app_name, account_label) = store
                .get(&p.app_id)
                .map(|a| {
                    let label = a
                        .accounts
                        .iter()
                        .find(|ac| ac.id == p.account_id)
                        .map(|ac| ac.label.clone())
                        .unwrap_or_default();
                    (a.name.clone(), label)
                })
                .unwrap_or_default();
            SplitPaneInfo {
                app_id: p.app_id.clone(),
                account_id: p.account_id.clone(),
                app_name,
                account_label,
            }
        })
        .collect();
    SplitInfo {
        id: group.id.clone(),
        layout: group.layout,
        panes,
        frame: group.frame,
        active_pane: group.active_pane,
    }
}

fn list_splits_inner(app: &AppHandle) -> Result<SplitList, String> {
    let store = app
        .try_state::<AppStore>()
        .ok_or_else(|| "app store unavailable".to_string())?;
    let splitstate = app
        .try_state::<SplitState>()
        .ok_or_else(|| "split state unavailable".to_string())?;
    let file = load_file(app);
    let open: Vec<SplitInfo> = splitstate
        .inner
        .lock()
        .map_err(|_| "split state lock poisoned".to_string())?
        .values()
        .map(|g| info_for(&store, g))
        .collect();
    Ok(SplitList {
        layouts: file.layouts,
        open,
    })
}

/// Reposition the bar and every pane of a group into the frame's
/// current size. Child webviews move with the frame natively; only
/// their bounds need setting, and only on resize. No state lock is
/// held here — the group is a snapshot.
fn retile(app: &AppHandle, group: &SplitGroup) {
    if let Some(bar) = app.get_webview(&format!("splitbar-{}", group.id)) {
        let _ = bar.set_bounds(Rect {
            position: Position::Logical(tauri::LogicalPosition::new(0.0, 0.0)),
            size: Size::Logical(tauri::LogicalSize::new(group.frame.w, SPLIT_BAR_H)),
        });
    }
    let rects = pane_rects(group.layout, group.frame.w, group.frame.h);
    for (pane, rect) in group.panes.iter().zip(rects) {
        if let Some(w) = app.get_webview(&pane_webview_label(&group.id, &pane.app_id, &pane.account_id)) {
            let _ = w.set_bounds(Rect {
                position: Position::Logical(tauri::LogicalPosition::new(rect.x, rect.y)),
                size: Size::Logical(tauri::LogicalSize::new(rect.w, rect.h)),
            });
        }
    }
}

/// Validate panes: count, existence, no repeats. Pure over the store so
/// it stays unit-testable without a Tauri app handle.
fn validate_panes(store: &AppStore, panes: &[SplitPane]) -> Result<(), String> {
    if panes.len() < MIN_PANES {
        return Err("Pick at least two apps for a split view.".to_string());
    }
    if panes.len() > MAX_PANES {
        return Err("A split view holds at most four apps.".to_string());
    }
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for p in panes {
        if p.app_id.is_empty() || p.account_id.is_empty() {
            return Err("That split has an empty app or account.".to_string());
        }
        if seen.contains(&(&p.app_id, &p.account_id)) {
            return Err("The same app account is in this split twice.".to_string());
        }
        seen.push((&p.app_id, &p.account_id));
        let ok = store
            .get(&p.app_id)
            .map(|a| a.accounts.iter().any(|ac| ac.id == p.account_id))
            .unwrap_or(false);
        if !ok {
            return Err("One of those apps or accounts no longer exists.".to_string());
        }
    }
    Ok(())
}

/// Default frame: 1280x800 centered on the primary monitor (logical px).
fn default_frame_on_monitor(app: &AppHandle) -> FrameRect {
    let mut frame = default_frame();
    if let Ok(Some(m)) = app.primary_monitor() {
        let pos = m.position();
        let size = m.size();
        let scale = m.scale_factor();
        let mw = size.width as f64 / scale;
        let mh = size.height as f64 / scale;
        frame.x = pos.x as f64 / scale + (mw - frame.w) / 2.0;
        frame.y = pos.y as f64 / scale + (mh - frame.h) / 2.0;
    }
    frame
}

/// Build the frame window for a group and wire it to the group's
/// lifetime: resize re-tiles the bar and panes, close closes the whole
/// group (pin-aware, on a worker thread — a pin confirm can block).
/// The frame starts hidden so the user never sees an empty window;
/// open_split shows it once the bar and panes are built. Child
/// webviews move with the frame natively, so Moved needs no handler.
fn build_frame(app: &AppHandle, group: &SplitGroup) -> Result<Window, String> {
    let frame = WindowBuilder::new(app, split_frame_label(&group.id))
        .title("Split view")
        .decorations(false)
        .background_color(tauri::utils::config::Color(0x1b, 0x1b, 0x1b, 0xff))
        .inner_size(group.frame.w, group.frame.h)
        .min_inner_size(640.0, 400.0)
        .position(group.frame.x, group.frame.y)
        .visible(false)
        .build()
        .map_err(|e| format!("could not open the split window: {e}"))?;
    // The bar is the frame's first child webview, across the top.
    let bar = frame
        .add_child(
            WebviewBuilder::new(
                format!("splitbar-{}", group.id),
                WebviewUrl::App("splitbar.html".into()),
            )
            .focused(false),
            Position::Logical(tauri::LogicalPosition::new(0.0, 0.0)),
            Size::Logical(tauri::LogicalSize::new(group.frame.w, SPLIT_BAR_H)),
        )
        .map_err(|e| format!("could not open the split bar: {e}"))?;
    let _ = bar;
    let ev_app = app.clone();
    let ev_group = group.id.clone();
    frame.on_window_event(move |event| match event {
        WindowEvent::Focused(focused) => {
            // v0.18.9: the frame is a plain Window whose bar and panes are
            // child webviews — window focus does not reach them. Focus the
            // active pane's webview so clicks land in the page.
            if *focused {
                #[cfg(windows)]
                focus_active_pane_webview(&ev_app, &ev_group);
            }
        }
        WindowEvent::Resized(_) => {
            // Read the frame's live size and re-tile. Position is the
            // window manager's business; only w/h change the layout.
            let size = ev_app
                .get_window(&split_frame_label(&ev_group))
                .and_then(|f| {
                    let scale = f.scale_factor().unwrap_or(1.0).max(0.1);
                    f.inner_size()
                        .ok()
                        .map(|s| (s.width as f64 / scale, s.height as f64 / scale))
                });
            if let Some((w, h)) = size {
                let group = ev_app.try_state::<SplitState>().and_then(|st| {
                    st.inner.lock().ok().and_then(|mut m| {
                        let g = m.get_mut(&ev_group)?;
                        g.frame.w = w;
                        g.frame.h = h;
                        Some(g.clone())
                    })
                });
                if let Some(group) = group {
                    retile(&ev_app, &group);
                }
            }
        }
        WindowEvent::CloseRequested { .. } => {
            // Pin confirms can block: close on a worker thread, never
            // inline (the tab strip's v0.13.1 deadlock, same shape).
            let c_app = ev_app.clone();
            let c_group = ev_group.clone();
            std::thread::Builder::new()
                .name("appmaka-split-close".to_string())
                .spawn(move || {
                    if let Some(st) = c_app.try_state::<SplitState>() {
                        let _ = close_split(&c_app, &st, &c_group);
                    }
                })
                .ok();
        }
        _ => {}
    });
    Ok(frame)
}

/// Open a split group: one frame window, the bar across its top, one
/// child webview per pane. An already-open account window is pulled
/// into the split (closed first — two webviews must never share one
/// session dir); a pinned one asks once, and a refusal stops the open
/// before anything is built. An account already live in another split
/// is refused outright. One failure rolls the whole group back.
pub fn open_split(
    app: &AppHandle,
    store: &AppStore,
    adblock: &crate::adblock::AdblockState,
    winstate: &crate::windows::WindowState,
    splitstate: &SplitState,
    panes: Vec<SplitPane>,
    layout: Option<SplitLayout>,
) -> Result<SplitInfo, String> {
    validate_panes(store, &panes)?;
    let layout = match layout {
        Some(l) => {
            if !layouts_for_count(panes.len()).contains(&l) {
                return Err("That layout doesn't fit this many apps.".to_string());
            }
            l
        }
        None => layout_for_count(panes.len())
            .ok_or_else(|| "A split view holds two to four apps.".to_string())?,
    };
    // One account, one live webview: an account already in another
    // split would share its session dir with this split's pane.
    for pane in &panes {
        if group_with_account(splitstate, &pane.app_id, &pane.account_id).is_some() {
            return Err("One of those apps is already open in another split view.".to_string());
        }
    }
    // Pull open standalone windows into the split. Pinned ones ask
    // once (the close-all pattern: one dialog, approvals land in
    // ConfirmedCloses for the close below); a refusal stops the open.
    let open_labels: Vec<String> = panes
        .iter()
        .map(|p| p.acct_label())
        .filter(|l| app.get_webview_window(l).is_some())
        .collect();
    if !open_labels.is_empty() {
        let approved = crate::pin::confirm_batch_close(app, &open_labels);
        for label in &open_labels {
            if crate::pin::is_pinned(app, label) && !approved.contains(label) {
                return Err("One of those apps is pinned in its own window. Unpin it first, then open the split view.".to_string());
            }
        }
        for label in &open_labels {
            crate::windows::close_tracked_window(app, label, CloseIntent::User);
        }
    }
    let group = SplitGroup {
        id: new_id(),
        layout,
        panes,
        frame: default_frame_on_monitor(app),
        active_pane: 0,
    };
    // Register first: a pane built below is already a member for the
    // duplicate check and the focus guard.
    splitstate
        .inner
        .lock()
        .map_err(|_| "split state lock poisoned".to_string())?
        .insert(group.id.clone(), group.clone());
    let frame = match build_frame(app, &group) {
        Ok(f) => f,
        Err(e) => {
            splitstate
                .inner
                .lock()
                .ok()
                .map(|mut m| m.remove(&group.id));
            return Err(e);
        }
    };
    // Panes, in order. One failure rolls the whole group back: built
    // panes close, the frame closes, the group is forgotten.
    let rects = pane_rects(group.layout, group.frame.w, group.frame.h);
    let mut opened: Vec<Webview> = Vec::new();
    for (pane, rect) in group.panes.iter().zip(rects) {
        match crate::windows::open_split_pane_webview(
            app,
            store,
            adblock,
            winstate,
            &frame,
            &pane_webview_label(&group.id, &pane.app_id, &pane.account_id),
            &pane.app_id,
            &pane.account_id,
            (rect.x, rect.y, rect.w, rect.h),
        ) {
            Ok(w) => opened.push(w),
            Err(e) => {
                for w in opened {
                    let _ = w.close();
                }
                let _ = frame.close();
                splitstate
                    .inner
                    .lock()
                    .ok()
                    .map(|mut m| m.remove(&group.id));
                return Err(format!("could not open a pane: {e}"));
            }
        }
    }
    // Show the frame (it was built hidden) and focus the first pane.
    // The frame takes focus first so its Focused event fires and the
    // active pane's webview gets focus through the frame handler too;
    // the explicit pane focus below covers the show-without-focus case.
    let _ = frame.show();
    let _ = frame.set_focus();
    // Re-apply the layout once the frame is visible and realized, in
    // case showing clobbered a bound. Main thread: GTK calls from
    // worker threads are not safe. NOTE (Linux): this changes nothing
    // there — Tauri's Linux runtime packs every child webview into the
    // frame's vertical box and ignores bounds entirely (wry
    // `add_to_container`, GtkBox branch), so panes stack full-width at
    // equal heights instead of tiling. Windows (SetWindowPos) and
    // macOS honor bounds; only Linux is affected.
    {
        let retile_app = app.clone();
        let retile_group = group.clone();
        let _ = app.run_on_main_thread(move || {
            retile(&retile_app, &retile_group);
        });
    }
    if let Some(first) = group.panes.first() {
        if let Some(w) = app.get_webview(&pane_webview_label(&group.id, &first.app_id, &first.account_id)) {
            let _ = w.set_focus();
        }
    }
    let info = info_for(store, &group);
    let _ = persist_open(app, splitstate);
    emit_changed(app);
    Ok(info)
}

/// Reopen a saved layout as a live split.
pub fn open_saved_layout(
    app: &AppHandle,
    store: &AppStore,
    adblock: &crate::adblock::AdblockState,
    winstate: &crate::windows::WindowState,
    splitstate: &SplitState,
    layout_id: &str,
) -> Result<SplitInfo, String> {
    let saved = load_file(app)
        .layouts
        .into_iter()
        .find(|l| l.id == layout_id)
        .ok_or_else(|| "That saved layout no longer exists.".to_string())?;
    open_split(app, store, adblock, winstate, splitstate, saved.panes, Some(saved.layout))
}

/// Close a split: pin-aware (one dialog for every pinned pane; a
/// refusal leaves the whole group open), then the group leaves state,
/// the panes close, and the frame closes. The snapshot is taken before
/// the pin check, so a refusal leaves the group exactly as it was.
pub fn close_split(
    app: &AppHandle,
    splitstate: &SplitState,
    group_id: &str,
) -> Result<(), String> {
    let group = splitstate
        .inner
        .lock()
        .map_err(|_| "split state lock poisoned".to_string())?
        .get(group_id)
        .cloned()
        .ok_or_else(|| "That split view is already gone.".to_string())?;
    let labels: Vec<String> = group.panes.iter().map(|p| p.acct_label()).collect();
    let approved = crate::pin::confirm_batch_close(app, &labels);
    for label in &labels {
        if crate::pin::is_pinned(app, label) && !approved.contains(label) {
            return Err("One of those apps is pinned. Unpin it first, then close the split view.".to_string());
        }
    }
    let group = splitstate
        .inner
        .lock()
        .map_err(|_| "split state lock poisoned".to_string())?
        .remove(group_id)
        .ok_or_else(|| "That split view is already gone.".to_string())?;
    // close_tracked_window consumes the pin approvals and prunes the
    // pane's renderer processes (panes aren't windows, so its own
    // window close is a no-op); the child webview closes after.
    for pane in &group.panes {
        crate::windows::close_tracked_window(app, &pane.acct_label(), CloseIntent::User);
    }
    for pane in &group.panes {
        if let Some(w) = app.get_webview(&pane_webview_label(&group.id, &pane.app_id, &pane.account_id)) {
            let _ = w.close();
        }
    }
    if let Some(frame) = app.get_window(&split_frame_label(&group.id)) {
        let _ = frame.close();
    }
    let _ = persist_open(app, splitstate);
    emit_changed(app);
    Ok(())
}

/// Switch a group's layout (only layouts that fit its pane count).
pub fn set_split_layout(
    app: &AppHandle,
    splitstate: &SplitState,
    group_id: &str,
    layout: SplitLayout,
) -> Result<SplitInfo, String> {
    let store = app
        .try_state::<AppStore>()
        .ok_or_else(|| "app store unavailable".to_string())?;
    let group = {
        let mut state = splitstate
            .inner
            .lock()
            .map_err(|_| "split state lock poisoned".to_string())?;
        let group = state
            .get_mut(group_id)
            .ok_or_else(|| "That split view is already gone.".to_string())?;
        if !layouts_for_count(group.panes.len()).contains(&layout) {
            return Err("That layout doesn't fit this many apps.".to_string());
        }
        group.layout = layout;
        group.clone()
    };
    retile(app, &group);
    let _ = persist_open(app, splitstate);
    emit_changed(app);
    Ok(info_for(&store, &group))
}

/// Close one pane of a group. Pin-aware: a pinned pane asks once, and
/// a refusal leaves the group untouched (Ok with the current info).
/// Survivors reflow; one pane left dissolves the group — the last pane
/// reopens as an ordinary standalone window. Returns the group's info,
/// or None when it dissolved.
pub fn remove_split_pane(
    app: &AppHandle,
    store: &AppStore,
    adblock: &crate::adblock::AdblockState,
    winstate: &crate::windows::WindowState,
    splitstate: &SplitState,
    group_id: &str,
    index: usize,
) -> Result<Option<SplitInfo>, String> {
    let group = splitstate
        .inner
        .lock()
        .map_err(|_| "split state lock poisoned".to_string())?
        .get(group_id)
        .cloned()
        .ok_or_else(|| "That split view is already gone.".to_string())?;
    if index >= group.panes.len() {
        return Err("That pane is already gone.".to_string());
    }
    let removed = group.panes[index].clone();
    // Dissolving (2 -> 1) closes the survivor's pane too, so its pin
    // counts: one batch dialog covers both, and a refusal is a no-op
    // for the whole group.
    let dissolving = group.panes.len() - 1 < MIN_PANES;
    let survivor = if dissolving {
        group
            .panes
            .iter()
            .enumerate()
            .find(|(i, _)| *i != index)
            .map(|(_, p)| p.clone())
    } else {
        None
    };
    let mut confirm_labels = vec![removed.acct_label()];
    if let Some(s) = &survivor {
        confirm_labels.push(s.acct_label());
    }
    let approved = crate::pin::confirm_batch_close(app, &confirm_labels);
    for label in &confirm_labels {
        if crate::pin::is_pinned(app, label) && !approved.contains(label) {
            return Ok(Some(info_for(store, &group)));
        }
    }
    let group = {
        let mut state = splitstate
            .inner
            .lock()
            .map_err(|_| "split state lock poisoned".to_string())?;
        let group = state
            .get_mut(group_id)
            .ok_or_else(|| "That split view is already gone.".to_string())?;
        group.panes.remove(index);
        group.clone()
    };
    // close_tracked_window consumes the pin approval and prunes the
    // pane's renderer processes; the child webview closes after.
    crate::windows::close_tracked_window(app, &removed.acct_label(), CloseIntent::User);
    if let Some(w) = app.get_webview(&pane_webview_label(&group.id, &removed.app_id, &removed.account_id)) {
        let _ = w.close();
    }
    if dissolving {
        // The group leaves state FIRST, so the focus guard in
        // open_account_placed can't misfire on the closing pane.
        let Some(last) = survivor else {
            return Ok(None);
        };
        splitstate
            .inner
            .lock()
            .ok()
            .map(|mut m| m.remove(group_id));
        // v0.18.10: the survivor reopens where it was, not on its home
        // page. Capture the pane's live URL and seed it before the
        // webview closes; open_account_placed consumes the seed one-shot.
        if let Some(url) = pane_live_url(app, group_id, &last) {
            if let Some(winstate) = app.try_state::<crate::windows::WindowState>() {
                winstate.seed_restore_url(&last.acct_label(), url);
            }
        }
        crate::windows::close_tracked_window(app, &last.acct_label(), CloseIntent::User);
        if let Some(w) = app.get_webview(&pane_webview_label(&group.id, &last.app_id, &last.account_id)) {
            let _ = w.close();
        }
        if let Some(frame) = app.get_window(&split_frame_label(group_id)) {
            let _ = frame.close();
        }
        let _ = persist_open(app, splitstate);
        emit_changed(app);
        // Fresh standalone window for the last pane (lands on its home
        // or last page — the same known restore limitation).
        let _ = crate::windows::open_account_placed(
            app, store, adblock, winstate, &last.app_id, &last.account_id, None, false, false,
        );
        return Ok(None);
    }
    let group = {
        let mut state = splitstate
            .inner
            .lock()
            .map_err(|_| "split state lock poisoned".to_string())?;
        let group = state
            .get_mut(group_id)
            .ok_or_else(|| "That split view is already gone.".to_string())?;
        group.layout = layout_for_count(group.panes.len()).unwrap_or(SplitLayout::TwoVertical);
        group.active_pane = group.active_pane.min(group.panes.len() - 1);
        group.clone()
    };
    retile(app, &group);
    let _ = persist_open(app, splitstate);
    emit_changed(app);
    Ok(Some(info_for(store, &group)))
}

/// Reorder panes inside a group (v0.18.10: drag a pane to swap
/// positions). The active pane follows its account, not its index.
/// Layout is unchanged (the count is unchanged); survivors reflow.
pub fn move_split_pane(
    app: &AppHandle,
    splitstate: &SplitState,
    group_id: &str,
    from: usize,
    to: usize,
) -> Result<SplitInfo, String> {
    let store = app
        .try_state::<AppStore>()
        .ok_or_else(|| "app store unavailable".to_string())?;
    let group = {
        let mut state = splitstate
            .inner
            .lock()
            .map_err(|_| "split state lock poisoned".to_string())?;
        let group = state
            .get_mut(group_id)
            .ok_or_else(|| "That split view is already gone.".to_string())?;
        if from >= group.panes.len() || to >= group.panes.len() {
            return Err("That pane is already gone.".to_string());
        }
        if from == to {
            return Ok(info_for(&store, group));
        }
        let active = group.panes.get(group.active_pane).cloned();
        let pane = group.panes.remove(from);
        group.panes.insert(to, pane);
        group.active_pane = active
            .and_then(|a| {
                group
                    .panes
                    .iter()
                    .position(|p| p.app_id == a.app_id && p.account_id == a.account_id)
            })
            .unwrap_or(0);
        group.clone()
    };
    retile(app, &group);
    let _ = persist_open(app, splitstate);
    emit_changed(app);
    Ok(info_for(&store, &group))
}

/// Pop one pane out of a group into its own standalone window
/// (v0.18.10: drag a pane out of the split). The pane reopens where it
/// was (live URL seeded before its webview closes); if the group had
/// two panes it dissolves and the survivor reopens standalone too.
pub fn pop_out_split_pane(
    app: &AppHandle,
    store: &AppStore,
    adblock: &crate::adblock::AdblockState,
    winstate: &crate::windows::WindowState,
    splitstate: &SplitState,
    group_id: &str,
    index: usize,
) -> Result<Option<SplitInfo>, String> {
    let group = splitstate
        .inner
        .lock()
        .map_err(|_| "split state lock poisoned".to_string())?
        .get(group_id)
        .cloned()
        .ok_or_else(|| "That split view is already gone.".to_string())?;
    if index >= group.panes.len() {
        return Err("That pane is already gone.".to_string());
    }
    let pane = group.panes[index].clone();
    // Seed the pane's live URL before anything closes, so the standalone
    // window lands where the pane was.
    if let Some(url) = pane_live_url(app, group_id, &pane) {
        winstate.seed_restore_url(&pane.acct_label(), url);
    }
    let result = remove_split_pane(app, store, adblock, winstate, splitstate, group_id, index)?;
    // remove_split_pane refuses pinned panes by returning the unchanged
    // group: only reopen standalone when the pane is actually gone.
    let popped = match &result {
        None => true,
        Some(info) => !info
            .panes
            .iter()
            .any(|p| p.app_id == pane.app_id && p.account_id == pane.account_id),
    };
    if popped {
        let _ = crate::windows::open_account_placed(
            app, store, adblock, winstate, &pane.app_id, &pane.account_id, None, false, false,
        );
    }
    Ok(result)
}

/// Add one app to a live split view (v0.18.10: pull an already-open
/// standalone window into an open split). The account's live URL is
/// seeded so the pane lands where its window was.
#[allow(clippy::too_many_arguments)] // pane identity is two strings; the other split fns carry the same six context args
pub fn add_pane_to_split(
    app: &AppHandle,
    store: &AppStore,
    adblock: &crate::adblock::AdblockState,
    winstate: &crate::windows::WindowState,
    splitstate: &SplitState,
    group_id: &str,
    app_id: &str,
    account_id: &str,
) -> Result<SplitInfo, String> {
    let pane = SplitPane {
        app_id: app_id.to_string(),
        account_id: account_id.to_string(),
    };
    let group = splitstate
        .inner
        .lock()
        .map_err(|_| "split state lock poisoned".to_string())?
        .get(group_id)
        .cloned()
        .ok_or_else(|| "That split view is already gone.".to_string())?;
    if group.panes.len() >= MAX_PANES {
        return Err("That split view is already full (four apps).".to_string());
    }
    if group
        .panes
        .iter()
        .any(|p| p.app_id == app_id && p.account_id == account_id)
    {
        return Err("That app is already in this split view.".to_string());
    }
    if let Some((other, _)) = group_with_account(splitstate, app_id, account_id) {
        if other != group_id {
            return Err("That app is already open in another split view.".to_string());
        }
    }
    // Validate the RESULTING pane list (count, existence, no repeats).
    // NOTE: do not validate the single new pane on its own — validate_panes
    // requires at least two panes, so a one-pane slice always fails with
    // "Pick at least two apps" (live-test catch, 2026-11-08).
    let mut candidate = group.panes.clone();
    candidate.push(pane.clone());
    validate_panes(store, &candidate)?;
    // Pull an open standalone window in: capture where it was, ask once
    // if pinned, then close it before building the pane (one account,
    // one live webview).
    let acct_label = pane.acct_label();
    if app.get_webview_window(&acct_label).is_some() {
        if let Some(window) = app.get_webview_window(&acct_label) {
            if let Ok(url) = window.url() {
                let url = url.to_string();
                if url != "about:blank" {
                    winstate.seed_restore_url(&acct_label, url);
                }
            }
        }
        let approved = crate::pin::confirm_batch_close(app, std::slice::from_ref(&acct_label));
        if crate::pin::is_pinned(app, &acct_label) && !approved.contains(&acct_label) {
            return Err("That app is pinned in its own window. Unpin it first, then add it to the split view.".to_string());
        }
        crate::windows::close_tracked_window(app, &acct_label, CloseIntent::User);
    }
    let frame_label = split_frame_label(group_id);
    let Some(frame) = app.get_window(&frame_label) else {
        return Err("That split view is already gone.".to_string());
    };
    let group = {
        let mut state = splitstate
            .inner
            .lock()
            .map_err(|_| "split state lock poisoned".to_string())?;
        let group = state
            .get_mut(group_id)
            .ok_or_else(|| "That split view is already gone.".to_string())?;
        group.panes.push(pane.clone());
        group.layout = layout_for_count(group.panes.len()).unwrap_or(SplitLayout::TwoVertical);
        group.active_pane = group.panes.len() - 1;
        group.clone()
    };
    let rects = pane_rects(group.layout, group.frame.w, group.frame.h);
    let rect = rects[group.panes.len() - 1];
    if let Err(e) = crate::windows::open_split_pane_webview(
        app,
        store,
        adblock,
        winstate,
        &frame,
        &pane_webview_label(&group.id, &pane.app_id, &pane.account_id),
        &pane.app_id,
        &pane.account_id,
        (rect.x, rect.y, rect.w, rect.h),
    ) {
        // Roll back the append; the group keeps its old layout and panes.
        if let Ok(mut state) = splitstate.inner.lock() {
            if let Some(g) = state.get_mut(group_id) {
                g.panes.retain(|p| !(p.app_id == pane.app_id && p.account_id == pane.account_id));
                g.layout = layout_for_count(g.panes.len()).unwrap_or(SplitLayout::TwoVertical);
                g.active_pane = g.active_pane.min(g.panes.len().saturating_sub(1));
            }
        }
        return Err(format!("could not add the pane: {e}"));
    }
    retile(app, &group);
    let _ = persist_open(app, splitstate);
    emit_changed(app);
    Ok(info_for(store, &group))
}

/// Focus one pane: show and focus the frame, focus the pane's child
/// webview, mark it active. No rebuild — the pane is already live.
/// v0.18.9: focus the active pane's webview without touching anything
/// else — no show, no state write, no emit. Used when the frame window
/// itself gains focus (Alt+Tab, taskbar): no pane-click happened, so no
/// pane has focus yet. Windows-only (see `windows::focus_webview`).
#[cfg(windows)]
fn focus_active_pane_webview(app: &AppHandle, group_id: &str) {
    let active = app.try_state::<SplitState>().and_then(|st| {
        st.inner.lock().ok().and_then(|m| {
            let g = m.get(group_id)?;
            g.panes
                .get(g.active_pane)
                .map(|p| (p.app_id.clone(), p.account_id.clone()))
        })
    });
    if let Some((app_id, account_id)) = active {
        let label = pane_webview_label(group_id, &app_id, &account_id);
        crate::windows::focus_webview(app, &label);
    }
}

pub fn focus_split_pane(
    app: &AppHandle,
    splitstate: &SplitState,
    group_id: &str,
    index: usize,
) -> Result<(), String> {
    let group = {
        let mut state = splitstate
            .inner
            .lock()
            .map_err(|_| "split state lock poisoned".to_string())?;
        let group = state
            .get_mut(group_id)
            .ok_or_else(|| "That split view is already gone.".to_string())?;
        if index >= group.panes.len() {
            return Err("That pane is already gone.".to_string());
        }
        group.active_pane = index;
        group.clone()
    };
    if let Some(frame) = app.get_window(&split_frame_label(group_id)) {
        let _ = frame.show();
        let _ = frame.set_focus();
    }
    let pane = &group.panes[index];
    if let Some(w) = app.get_webview(&pane_webview_label(group_id, &pane.app_id, &pane.account_id)) {
        let _ = w.set_focus();
    }
    // Focusing a pane refreshes its idle clock and wakes it if the
    // watcher shed its memory.
    touch_pane_activity(
        app,
        &pane_webview_label(group_id, &pane.app_id, &pane.account_id),
    );
    let _ = persist_open(app, splitstate);
    emit_changed(app);
    Ok(())
}

/// The live URL of a pane's webview, if it has navigated somewhere
/// readable. about:blank is not a destination — callers must not seed
/// it as a restore URL, or the reopened window lands blank.
fn pane_live_url(app: &AppHandle, group_id: &str, pane: &SplitPane) -> Option<String> {
    let label = pane_webview_label(group_id, &pane.app_id, &pane.account_id);
    let url = app.get_webview(&label).and_then(|w| w.url().ok())?;
    let url = url.to_string();
    if url == "about:blank" {
        return None;
    }
    Some(url)
}

/// v0.18.10: mark the pane holding this webview active, if it isn't
/// already. Called from the gesture transport (a pointerdown/keydown
/// inside a pane), so the bar's highlight follows clicks inside pages,
/// not just bar clicks. Writes only on a real change.
pub fn mark_pane_active_for_webview(app: &AppHandle, webview_label: &str) {
    let Some(splitstate) = app.try_state::<SplitState>() else {
        return;
    };
    let changed = {
        let mut state = match splitstate.inner.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut changed = false;
        for group in state.values_mut() {
            if let Some(i) = group.panes.iter().position(|p| {
                pane_webview_label(&group.id, &p.app_id, &p.account_id) == webview_label
            }) {
                if group.active_pane != i {
                    group.active_pane = i;
                    changed = true;
                }
                break;
            }
        }
        changed
    };
    if changed {
        let _ = persist_open(app, &splitstate);
        emit_changed(app);
    }
}

/// The live split group holding this account, if any, with the pane's
/// index. open_split refuses a duplicate; open_account_placed focuses
/// it instead of building a second webview on the session dir.
pub fn group_with_account(
    splitstate: &SplitState,
    app_id: &str,
    account_id: &str,
) -> Option<(String, usize)> {
    let state = splitstate.inner.lock().ok()?;
    for (id, group) in state.iter() {
        if let Some(i) = group
            .panes
            .iter()
            .position(|p| p.app_id == app_id && p.account_id == account_id)
        {
            return Some((id.clone(), i));
        }
    }
    None
}

/// Focus the split pane holding this account, if one is live. True
/// when a pane was focused (the caller must not open a second webview
/// on the account's session dir).
pub fn focus_pane_for_account(app: &AppHandle, app_id: &str, account_id: &str) -> bool {
    let Some(splitstate) = app.try_state::<SplitState>() else {
        return false;
    };
    let Some((group_id, index)) = group_with_account(&splitstate, app_id, account_id) else {
        return false;
    };
    focus_split_pane(app, &splitstate, &group_id, index).is_ok()
}

/// Save an open group as a named layout. Re-saving the same group
/// updates the layout with that name instead of duplicating it.
pub fn save_split_layout(
    app: &AppHandle,
    store: &AppStore,
    splitstate: &SplitState,
    name: String,
    group_id: &str,
) -> Result<SplitList, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Give the layout a name.".to_string());
    }
    if name.chars().count() > 60 {
        return Err("Name is too long (max 60 characters).".to_string());
    }
    let group = splitstate
        .inner
        .lock()
        .map_err(|_| "split state lock poisoned".to_string())?
        .get(group_id)
        .cloned()
        .ok_or_else(|| "That split view is already gone.".to_string())?;
    validate_panes(store, &group.panes)?;
    let _guard = lock_file()?;
    let mut file = load_file(app);
    match file.layouts.iter_mut().find(|l| l.name == name) {
        Some(slot) => {
            slot.layout = group.layout;
            slot.panes = group.panes;
        }
        None => file.layouts.push(SavedSplitLayout {
            id: new_id(),
            name,
            layout: group.layout,
            panes: group.panes,
        }),
    }
    file.layouts.sort_by_key(|l| l.name.to_lowercase());
    save_file(app, &file)?;
    list_splits_inner(app)
}

/// Delete a saved layout (open groups are untouched).
pub fn delete_split_layout(app: &AppHandle, layout_id: &str) -> Result<SplitList, String> {
    let _guard = lock_file()?;
    let mut file = load_file(app);
    let before = file.layouts.len();
    file.layouts.retain(|l| l.id != layout_id);
    if file.layouts.len() == before {
        return Err("That saved layout is already gone.".to_string());
    }
    save_file(app, &file)?;
    list_splits_inner(app)
}

/// List saved layouts plus open groups.
pub fn list_splits(app: &AppHandle) -> Result<SplitList, String> {
    list_splits_inner(app)
}

/// Reopen the splits that were open at shutdown, after the session
/// restore. Best-effort per group: one bad group never stops the rest.
pub fn restore_open_splits(app: &AppHandle) {
    let (Some(store), Some(adblock), Some(winstate), Some(splitstate)) = (
        app.try_state::<AppStore>(),
        app.try_state::<crate::adblock::AdblockState>(),
        app.try_state::<crate::windows::WindowState>(),
        app.try_state::<SplitState>(),
    ) else {
        return;
    };
    for group in load_file(app).open {
        if validate_panes(&store, &group.panes).is_err() {
            continue;
        }
        let _ = open_split(
            app,
            &store,
            &adblock,
            &winstate,
            &splitstate,
            group.panes,
            Some(group.layout),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(app: &str, account: &str) -> SplitPane {
        SplitPane {
            app_id: app.to_string(),
            account_id: account.to_string(),
        }
    }

    #[test]
    fn frame_and_pane_labels_derive_from_group_id() {
        assert_eq!(split_frame_label("sp-1"), "split-sp-1");
        assert_eq!(
            pane_webview_label("sp-1", "app-1", "main"),
            "splitpane-sp-1-app-1-main"
        );
    }

    #[test]
    fn layout_matches_pane_count() {
        assert_eq!(layout_for_count(2), Some(SplitLayout::TwoVertical));
        assert_eq!(layout_for_count(3), Some(SplitLayout::Three));
        assert_eq!(layout_for_count(4), Some(SplitLayout::Four));
        assert_eq!(layout_for_count(1), None);
        assert_eq!(layout_for_count(5), None);
    }

    #[test]
    fn two_vertical_tiles_side_by_side() {
        let rects = pane_rects(SplitLayout::TwoVertical, 1200.0, 800.0);
        assert_eq!(rects.len(), 2);
        // Below the bar (y=36), equal halves, no gap, frame-relative.
        assert_eq!(rects[0], FrameRect { x: 0.0, y: 36.0, w: 600.0, h: 764.0 });
        assert_eq!(rects[1], FrameRect { x: 600.0, y: 36.0, w: 600.0, h: 764.0 });
    }

    #[test]
    fn two_horizontal_tiles_stacked() {
        let rects = pane_rects(SplitLayout::TwoHorizontal, 1200.0, 800.0);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0], FrameRect { x: 0.0, y: 36.0, w: 1200.0, h: 382.0 });
        assert_eq!(rects[1], FrameRect { x: 0.0, y: 418.0, w: 1200.0, h: 382.0 });
    }

    #[test]
    fn three_tiles_one_tall_plus_two_stacked() {
        let rects = pane_rects(SplitLayout::Three, 1200.0, 800.0);
        assert_eq!(rects.len(), 3);
        assert_eq!(rects[0], FrameRect { x: 0.0, y: 36.0, w: 600.0, h: 764.0 });
        assert_eq!(rects[1], FrameRect { x: 600.0, y: 36.0, w: 600.0, h: 382.0 });
        assert_eq!(rects[2], FrameRect { x: 600.0, y: 418.0, w: 600.0, h: 382.0 });
    }

    #[test]
    fn four_tiles_two_by_two() {
        let rects = pane_rects(SplitLayout::Four, 1200.0, 800.0);
        assert_eq!(rects.len(), 4);
        assert_eq!(rects[0], FrameRect { x: 0.0, y: 36.0, w: 600.0, h: 382.0 });
        assert_eq!(rects[1], FrameRect { x: 600.0, y: 36.0, w: 600.0, h: 382.0 });
        assert_eq!(rects[2], FrameRect { x: 0.0, y: 418.0, w: 600.0, h: 382.0 });
        assert_eq!(rects[3], FrameRect { x: 600.0, y: 418.0, w: 600.0, h: 382.0 });
    }

    #[test]
    fn pane_labels_derive_from_app_and_account() {
        assert_eq!(pane("app-1", "main").acct_label(), "acct-app-1-main");
        assert_eq!(
            pane_webview_label("sp-1", "app-1", "main"),
            "splitpane-sp-1-app-1-main"
        );
        assert_eq!(split_frame_label("sp-1"), "split-sp-1");
    }
}