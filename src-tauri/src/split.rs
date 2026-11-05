//! Split view (v0.18.7): up to four apps side by side in one window.
//!
//! A split is a *group* of ordinary account windows, not one window with
//! several webviews: Tauri's multi-webview API (`Window::add_child`)
//! needs the `unstable` cargo feature, which this repo deliberately does
//! not use, and tabbed windows already proved the one-live-webview-per-
//! window design (tabs.rs). So each pane is a normal `acct-*` window —
//! same session dir, same adblock, same caption strip — tiled by a
//! shared bar window across the top, exactly like the tab strip docks
//! above a tabbed window.
//!
//! The bar is the group's anchor: dragging it moves every pane,
//! resizing it re-tiles them, minimizing it hides them, closing it
//! closes them all (pin-aware, through the normal account-close path).
//! Panes keep their own caption strips, so each pane navigates on its
//! own; the bar holds only group controls (layout, close all).
//!
//! Layout is a function of pane count, never a free choice: 2 panes are
//! side-by-side or stacked, 3 panes are one tall + two stacked, 4 panes
//! are a 2x2 grid. Removing a pane reflows the survivors and re-picks
//! the layout; one pane left dissolves the group (the pane stays open
//! as an ordinary window).
//!
//! Saved layouts and open groups persist in `<app-data>/splits.json`
//! (the workspaces.rs pattern: load-or-default, tmp+rename, file lock).
//! Open groups restore at startup after the session restore — session
//! writes skip split panes (session.rs), so a split never comes back as
//! four small standalone windows.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, Position, Size, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

use crate::pin::CloseIntent;
use crate::store::AppStore;
use crate::windows::{account_window_label, WindowPlacement};

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

/// One pane: an (app, account) pair. The live window label derives from
/// it (`account_window_label`), exactly like standalone windows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SplitPane {
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub account_id: String,
}

impl SplitPane {
    pub fn label(&self) -> String {
        account_window_label(&self.app_id, &self.account_id)
    }
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

/// Merge a bar move/resize into the group frame: the bar spans the
/// frame's top edge, so its x/y/w ARE the frame's — but its height (36)
/// is not. Storing the bar's own rect as the frame collapses the pane
/// area to zero height and strands the panes (GTK refuses a zero-height
/// resize but still applies the move).
fn frame_after_bar_move(frame: &FrameRect, bar_x: f64, bar_y: f64, bar_w: f64) -> FrameRect {
    FrameRect {
        x: bar_x,
        y: bar_y,
        w: bar_w,
        h: frame.h,
    }
}

/// Pure tiling: pane rects (logical, below the bar) for a frame. The
/// rects tile the pane area exactly — no gaps, no overlap.
pub fn pane_rects(layout: SplitLayout, frame: &FrameRect) -> Vec<FrameRect> {
    let area = FrameRect {
        x: frame.x,
        y: frame.y + SPLIT_BAR_H,
        w: frame.w,
        h: (frame.h - SPLIT_BAR_H).max(0.0),
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

/// Bar window label for a group.
pub fn split_bar_label(group_id: &str) -> String {
    format!("splitbar-{group_id}")
}

/// Reposition every pane of a group into the frame's current rect. No
/// state lock is held here — the group is a snapshot.
fn retile(app: &AppHandle, group: &SplitGroup) {
    let rects = pane_rects(group.layout, &group.frame);
    for (pane, rect) in group.panes.iter().zip(rects) {
        if let Some(w) = app.get_webview_window(&pane.label()) {
            let _ = w.set_position(Position::Logical(tauri::LogicalPosition::new(rect.x, rect.y)));
            let _ = w.set_size(Size::Logical(tauri::LogicalSize::new(rect.w, rect.h)));
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

/// Build the bar window for a group and wire it to the group's lifetime:
/// move/resize re-tiles the panes, minimize hides them, close closes the
/// whole group (pin-aware, on a worker thread — a pin confirm can block).
fn build_bar(app: &AppHandle, group: &SplitGroup) -> Result<WebviewWindow, String> {
    let bar = WebviewWindowBuilder::new(
        app,
        split_bar_label(&group.id),
        WebviewUrl::App("splitbar.html".into()),
    )
    .title("Split view")
    .decorations(false)
    .skip_taskbar(true)
    .focused(false)
    .inner_size(group.frame.w, SPLIT_BAR_H)
    .position(group.frame.x, group.frame.y)
    .build()
    .map_err(|e| format!("could not open the split bar: {e}"))?;
    let ev_app = app.clone();
    let ev_group = group.id.clone();
    bar.on_window_event(move |event| match event {
        WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
            // Read the bar's live x/y/w and re-tile. The bar spans the
            // frame's top edge, so its x/y/w ARE the frame's — but its
            // height (36) is NOT the frame's height. Storing the bar's
            // own rect as the frame collapses the pane area to zero
            // height (GTK then refuses every pane resize and the panes
            // strand at their old sizes in their new positions), so
            // only x/y/w move; the stored height stands.
            let bar_xyw = ev_app
                .get_webview_window(&split_bar_label(&ev_group))
                .and_then(|b| {
                    let scale = b.scale_factor().unwrap_or(1.0).max(0.1);
                    match (b.outer_position(), b.outer_size()) {
                        (Ok(p), Ok(s)) => Some((
                            p.x as f64 / scale,
                            p.y as f64 / scale,
                            s.width as f64 / scale,
                        )),
                        _ => None,
                    }
                });
            if let Some((bx, by, bw)) = bar_xyw {
                let group = ev_app.try_state::<SplitState>().and_then(|st| {
                    st.inner.lock().ok().and_then(|mut m| {
                        let g = m.get_mut(&ev_group)?;
                        g.frame = frame_after_bar_move(&g.frame, bx, by, bw);
                        Some(g.clone())
                    })
                });
                if let Some(group) = group {
                    retile(&ev_app, &group);
                }
            }
        }
        WindowEvent::Focused(focused) => {
            // Minimize hides the panes; plain focus loss keeps them.
            // Same rule as the tab strip (tabs.rs).
            let panes: Vec<String> = ev_app
                .try_state::<SplitState>()
                .and_then(|st| {
                    st.inner.lock().ok().and_then(|m| {
                        m.get(&ev_group)
                            .map(|g| g.panes.iter().map(|p| p.label()).collect())
                    })
                })
                .unwrap_or_default();
            if *focused {
                for label in panes {
                    if let Some(w) = ev_app.get_webview_window(&label) {
                        let _ = w.show();
                    }
                }
            } else if ev_app
                .get_webview_window(&split_bar_label(&ev_group))
                .and_then(|w| w.is_minimized().ok())
                .unwrap_or(false)
            {
                for label in panes {
                    if let Some(w) = ev_app.get_webview_window(&label) {
                        let _ = w.hide();
                    }
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
    Ok(bar)
}

/// Open a split group: bar plus panes. Panes open through the normal
/// account path, so sessions, adblock, and caption strips all apply.
/// An already-open account window is pulled into the grid (its
/// placement moves it); closing the split closes its panes.
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
    let group = SplitGroup {
        id: new_id(),
        layout,
        panes,
        frame: default_frame_on_monitor(app),
    };
    // Register first: panes opened below are already members for the
    // Destroyed handler and the session skip.
    splitstate
        .inner
        .lock()
        .map_err(|_| "split state lock poisoned".to_string())?
        .insert(group.id.clone(), group.clone());
    let bar = match build_bar(app, &group) {
        Ok(b) => b,
        Err(e) => {
            splitstate
                .inner
                .lock()
                .ok()
                .map(|mut m| m.remove(&group.id));
            return Err(e);
        }
    };
    let _ = bar;
    // Panes, in order. One failure rolls the whole group back: opened
    // panes close, the bar closes, the group is forgotten.
    let rects = pane_rects(group.layout, &group.frame);
    let mut opened: Vec<String> = Vec::new();
    for (pane, rect) in group.panes.iter().zip(rects) {
        let placement = WindowPlacement {
            x: rect.x,
            y: rect.y,
            width: rect.w,
            height: rect.h,
        };
        match crate::windows::open_account_placed(
            app, store, adblock, winstate, &pane.app_id, &pane.account_id, Some(placement), false,
            false,
        ) {
            Ok(()) => opened.push(pane.label()),
            Err(e) => {
                for label in opened {
                    crate::windows::close_tracked_window(app, &label, CloseIntent::User);
                }
                if let Some(b) = app.get_webview_window(&split_bar_label(&group.id)) {
                    let _ = b.close();
                }
                splitstate
                    .inner
                    .lock()
                    .ok()
                    .map(|mut m| m.remove(&group.id));
                return Err(format!("could not open a pane: {e}"));
            }
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

/// Close a split: every pane through the normal pin-aware close, then
/// the bar. Taking the group out of state first makes the panes'
/// Destroyed handlers no-ops by construction.
pub fn close_split(
    app: &AppHandle,
    splitstate: &SplitState,
    group_id: &str,
) -> Result<(), String> {
    let group = splitstate
        .inner
        .lock()
        .map_err(|_| "split state lock poisoned".to_string())?
        .remove(group_id)
        .ok_or_else(|| "That split view is already gone.".to_string())?;
    for pane in &group.panes {
        crate::windows::close_tracked_window(app, &pane.label(), CloseIntent::User);
    }
    if let Some(bar) = app.get_webview_window(&split_bar_label(group_id)) {
        let _ = bar.close();
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

/// Close one pane of a group. Survivors reflow; one pane left dissolves
/// the group (the pane stays open as an ordinary window). Returns the
/// group's info, or None when it dissolved.
pub fn remove_split_pane(
    app: &AppHandle,
    splitstate: &SplitState,
    group_id: &str,
    index: usize,
) -> Result<Option<SplitInfo>, String> {
    let store = app
        .try_state::<AppStore>()
        .ok_or_else(|| "app store unavailable".to_string())?;
    let (group, removed_label) = {
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
        let removed = group.panes.remove(index).label();
        (group.clone(), removed)
    };
    crate::windows::close_tracked_window(app, &removed_label, CloseIntent::User);
    if group.panes.len() < MIN_PANES {
        // Dissolve: the last pane stays open as an ordinary window.
        splitstate
            .inner
            .lock()
            .ok()
            .map(|mut m| m.remove(group_id));
        if let Some(bar) = app.get_webview_window(&split_bar_label(group_id)) {
            let _ = bar.close();
        }
        let _ = persist_open(app, splitstate);
        emit_changed(app);
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
        group.clone()
    };
    retile(app, &group);
    let _ = persist_open(app, splitstate);
    emit_changed(app);
    Ok(Some(info_for(&store, &group)))
}

/// Focus one pane (brings its window forward, in place — no move).
pub fn focus_split_pane(
    app: &AppHandle,
    store: &AppStore,
    adblock: &crate::adblock::AdblockState,
    winstate: &crate::windows::WindowState,
    splitstate: &SplitState,
    group_id: &str,
    index: usize,
) -> Result<(), String> {
    let pane = splitstate
        .inner
        .lock()
        .map_err(|_| "split state lock poisoned".to_string())?
        .get(group_id)
        .and_then(|g| g.panes.get(index).cloned())
        .ok_or_else(|| "That pane is already gone.".to_string())?;
    crate::windows::open_account_placed(
        app, store, adblock, winstate, &pane.app_id, &pane.account_id, None, false, false,
    )
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

/// True when this window label belongs to a split pane. Session writes
/// consult it so split panes never restore as standalone windows.
pub fn is_split_pane(app: &AppHandle, label: &str) -> bool {
    app.try_state::<SplitState>()
        .and_then(|st| {
            st.inner.lock().ok().map(|m| {
                m.values()
                    .any(|g| g.panes.iter().any(|p| p.label() == label))
            })
        })
        .unwrap_or(false)
}

/// A pane window was destroyed (its own X, or a close from elsewhere):
/// drop it from its group, reflow the survivors, dissolve the group
/// when one pane is left. Called from the account Destroyed handler.
/// No state lock is held across any window call.
pub fn on_pane_window_destroyed(app: &AppHandle, label: &str) {
    let Some(splitstate) = app.try_state::<SplitState>() else {
        return;
    };
    let plan = {
        let mut state = match splitstate.inner.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut found: Option<(String, SplitGroup)> = None;
        for (id, group) in state.iter_mut() {
            if let Some(idx) = group.panes.iter().position(|p| p.label() == label) {
                group.panes.remove(idx);
                found = Some((id.clone(), group.clone()));
                break;
            }
        }
        found
    };
    let Some((group_id, group)) = plan else {
        return;
    };
    if group.panes.len() < MIN_PANES {
        // Dissolve: remove the group, close the bar, leave the last
        // pane (if any) open as an ordinary window.
        if let Ok(mut state) = splitstate.inner.lock() {
            state.remove(&group_id);
        }
        if let Some(bar) = app.get_webview_window(&split_bar_label(&group_id)) {
            let _ = bar.close();
        }
        let _ = persist_open(app, &splitstate);
        emit_changed(app);
        return;
    }
    let group = {
        let mut state = match splitstate.inner.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        let Some(group) = state.get_mut(&group_id) else {
            return;
        };
        group.layout = layout_for_count(group.panes.len()).unwrap_or(SplitLayout::TwoVertical);
        group.clone()
    };
    retile(app, &group);
    let _ = persist_open(app, &splitstate);
    emit_changed(app);
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

    fn frame() -> FrameRect {
        FrameRect {
            x: 100.0,
            y: 100.0,
            w: 1200.0,
            h: 800.0,
        }
    }

    #[test]
    fn bar_label_prefixes_group_id() {
        assert_eq!(split_bar_label("sp-1"), "splitbar-sp-1");
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
        let rects = pane_rects(SplitLayout::TwoVertical, &frame());
        assert_eq!(rects.len(), 2);
        // Below the bar (y=136), equal halves, no gap.
        assert_eq!(rects[0], FrameRect { x: 100.0, y: 136.0, w: 600.0, h: 764.0 });
        assert_eq!(rects[1], FrameRect { x: 700.0, y: 136.0, w: 600.0, h: 764.0 });
    }

    #[test]
    fn two_horizontal_tiles_stacked() {
        let rects = pane_rects(SplitLayout::TwoHorizontal, &frame());
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0], FrameRect { x: 100.0, y: 136.0, w: 1200.0, h: 382.0 });
        assert_eq!(rects[1], FrameRect { x: 100.0, y: 518.0, w: 1200.0, h: 382.0 });
    }

    #[test]
    fn three_tiles_one_tall_plus_two_stacked() {
        let rects = pane_rects(SplitLayout::Three, &frame());
        assert_eq!(rects.len(), 3);
        assert_eq!(rects[0], FrameRect { x: 100.0, y: 136.0, w: 600.0, h: 764.0 });
        assert_eq!(rects[1], FrameRect { x: 700.0, y: 136.0, w: 600.0, h: 382.0 });
        assert_eq!(rects[2], FrameRect { x: 700.0, y: 518.0, w: 600.0, h: 382.0 });
    }

    #[test]
    fn bar_move_keeps_frame_height() {
        // Live-test regression (2026-11-05): the bar handler stored the
        // bar's own 36px rect as the frame, collapsing the pane area to
        // zero height — GTK refused the resizes, the panes stranded at
        // their old sizes in their new positions.
        let merged = frame_after_bar_move(&frame(), 120.0, 60.0, 1100.0);
        assert_eq!(
            merged,
            FrameRect {
                x: 120.0,
                y: 60.0,
                w: 1100.0,
                h: 800.0
            }
        );
    }

    #[test]
    fn four_tiles_two_by_two() {
        let rects = pane_rects(SplitLayout::Four, &frame());
        assert_eq!(rects.len(), 4);
        assert_eq!(rects[0], FrameRect { x: 100.0, y: 136.0, w: 600.0, h: 382.0 });
        assert_eq!(rects[1], FrameRect { x: 700.0, y: 136.0, w: 600.0, h: 382.0 });
        assert_eq!(rects[2], FrameRect { x: 100.0, y: 518.0, w: 600.0, h: 382.0 });
        assert_eq!(rects[3], FrameRect { x: 700.0, y: 518.0, w: 600.0, h: 382.0 });
    }

    #[test]
    fn pane_labels_derive_from_app_and_account() {
        assert_eq!(pane("app-1", "main").label(), "acct-app-1-main");
    }
}
