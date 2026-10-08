//! Routines ("morning stack"): one keystroke opens a chosen set of apps AND
//! the right accounts. Example: "Morning" opens work Gmail + the user's main
//! Muse account + a dashboard.
//!
//! Stored in `<app-data>/routines.json` following the same load/save pattern
//! as `custom_programs.rs` (atomic write via a temp file + rename). Routines
//! are inert config — zero idle cost — until the user runs one. Running a
//! routine opens windows, so `run_routine` MUST stay `async fn` (building a
//! window inside a synchronous command deadlocks WebView2 — see the note on
//! `open_account` in main.rs, wry#583).
//!
//! Optional per-routine global hotkeys are registered through the shared
//! `hotkeys` module (owned by a sibling worker in v0.8.0) under the binding
//! id `routine:<routine-id>`:
//!   `pub fn set_binding(app: &AppHandle, binding_id: &str, kind: HotkeyKind,
//!                       hotkey: &str) -> Result<(), String>`
//!   `pub fn remove_binding(app: &AppHandle, binding_id: &str) -> Result<(), String>`
//! Conflict failures come back as plain-language strings and are surfaced
//! as-is. THIS FILE WILL NOT COMPILE UNTIL `src-tauri/src/hotkeys.rs` lands
//! — that is expected; the coordinator integrates both together.
//!
//! NOTE (coordinator integration):
//! - add `mod routines;` to main.rs (next to `mod hotkeys;`);
//! - register `routines::list_routines`, `routines::save_routine`,
//!   `routines::delete_routine`, and `routines::run_routine` in the
//!   `invoke_handler` list;
//! - JS: `invoke("list_routines")`, `invoke("save_routine", { routine })`,
//!   `invoke("delete_routine", { routineId })`, `invoke("run_routine", { routineId })`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State};

use crate::adblock::AdblockState;
use crate::hotkeys::{remove_binding, set_binding, HotkeyKind};
use crate::launcher::LauncherState;
use crate::store::AppStore;
use crate::windows::WindowState;

/// One item inside a routine: either an (app, account) pair to open in an
/// account window, or a desktop program to launch natively.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutineItem {
    /// `"account"` or `"program"`.
    #[serde(default)]
    pub kind: String,
    /// Web app id (for `"account"` items).
    #[serde(default)]
    pub app_id: String,
    /// Account id (for `"account"` items).
    #[serde(default)]
    pub account_id: Option<String>,
    /// Native program id (for `"program"` items).
    #[serde(default)]
    pub program_id: Option<String>,
}

/// How a routine arranges the windows it opens (v0.9.0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoutineLayout {
    /// Windows open cascaded (overlapping) — the behavior before v0.9.0.
    #[default]
    Cascade,
    /// Account windows tile as equal side-by-side columns ("pillars")
    /// across the cursor's monitor, left to right in routine order.
    SideBySide,
    /// v0.13.0: account items open as tabs in a single tabbed window,
    /// in routine order. Program items still launch natively.
    Tabbed,
}

/// A named, hotkey-able set of things to open together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Routine {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Optional global hotkey (e.g. `"Ctrl+Alt+M"`). None = no hotkey.
    #[serde(default)]
    pub hotkey: Option<String>,
    #[serde(default)]
    pub items: Vec<RoutineItem>,
    /// Window layout for this routine's account windows. Defaults to
    /// Cascade so routines saved before v0.9.0 keep their behavior.
    #[serde(default)]
    pub layout: RoutineLayout,
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Binding id under which this routine's hotkey is registered with the
/// shared hotkeys module. Kept in one place so save/delete/run agree.
pub fn binding_id(routine_id: &str) -> String {
    format!("routine:{routine_id}")
}

fn routines_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("routines.json"))
}

/// Load-or-empty: a missing or corrupt file is just "no routines".
pub fn load(app: &AppHandle) -> Vec<Routine> {
    routines_path(app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|c| serde_json::from_str::<Vec<Routine>>(&c).ok())
        .map(|v| v.into_iter().filter(|r| !r.id.is_empty()).collect())
        .unwrap_or_default()
}

fn save(app: &AppHandle, list: &[Routine]) -> Result<(), String> {
    let path = routines_path(app)?;
    let json =
        serde_json::to_string_pretty(list).map_err(|e| format!("could not encode: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| format!("could not write: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("could not save: {e}"))?;
    Ok(())
}

/// Pure upsert: replaces the routine with the same id, or appends.
/// Extracted for testing (no AppHandle needed).
fn upsert(list: &mut Vec<Routine>, routine: Routine) {
    if let Some(slot) = list.iter_mut().find(|r| r.id == routine.id) {
        *slot = routine;
    } else {
        list.push(routine);
    }
}

fn validate(routine: &Routine) -> Result<(), String> {
    if routine.name.trim().is_empty() {
        return Err("Give the routine a name.".to_string());
    }
    if routine.name.chars().count() > 60 {
        return Err("Name is too long (max 60 characters).".to_string());
    }
    if routine.items.is_empty() {
        return Err("Add at least one app, account, or program.".to_string());
    }
    if routine.items.len() > 30 {
        return Err("Too many items (max 30).".to_string());
    }
    for item in &routine.items {
        match item.kind.as_str() {
            "account" => {
                if item.app_id.trim().is_empty() || item.account_id.as_deref().map(str::trim).unwrap_or("").is_empty() {
                    return Err("Every account item needs an app and an account.".to_string());
                }
            }
            "program" => {
                if item.program_id.as_deref().map(str::trim).unwrap_or("").is_empty() {
                    return Err("Every program item needs a program.".to_string());
                }
            }
            other => {
                return Err(format!("Unknown routine item kind \"{other}\"."));
            }
        }
    }
    Ok(())
}

/// Register (or remove) the routine's global hotkey through the shared
/// hotkeys module. Called by save/delete; conflict errors surface verbatim.
fn sync_hotkey(app: &AppHandle, routine: &Routine) -> Result<(), String> {
    let id = binding_id(&routine.id);
    match routine.hotkey.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        Some(hotkey) => set_binding(
            app,
            &id,
            HotkeyKind::Routine {
                routine_id: routine.id.clone(),
            },
            hotkey,
        ),
        // No hotkey on the routine: make sure no stale binding survives.
        None => {
            let _ = remove_binding(app, &id);
            Ok(())
        }
    }
}

/// List all routines. JS: `invoke("list_routines")`.
#[tauri::command]
pub fn list_routines(app: AppHandle) -> Result<Vec<Routine>, String> {
    Ok(load(&app))
}

/// Create or update a routine (upsert by id; empty id gets a fresh one).
/// Also syncs the routine's global hotkey binding. Returns the full list.
/// JS: `invoke("save_routine", { routine })`.
#[tauri::command]
pub fn save_routine(app: AppHandle, routine: Routine) -> Result<Vec<Routine>, String> {
    let mut routine = routine;
    routine.name = routine.name.trim().to_string();
    if routine.id.trim().is_empty() {
        routine.id = format!("routine-{}", unix_millis());
    }
    validate(&routine)?;
    // Register the hotkey FIRST: if it conflicts we bail without persisting
    // a hotkey that was never actually registered.
    sync_hotkey(&app, &routine)?;
    let mut list = load(&app);
    upsert(&mut list, routine);
    save(&app, &list)?;
    Ok(list)
}

/// Delete a routine and remove its hotkey binding. Returns the full list.
/// JS: `invoke("delete_routine", { routineId })`.
#[tauri::command]
pub fn delete_routine(app: AppHandle, routine_id: String) -> Result<Vec<Routine>, String> {
    let id = binding_id(&routine_id);
    // Best effort: the routine may never have had a binding.
    let _ = remove_binding(&app, &id);
    let mut list = load(&app);
    let before = list.len();
    list.retain(|r| r.id != routine_id);
    if list.len() == before {
        return Err("That routine is already gone.".to_string());
    }
    save(&app, &list)?;
    Ok(list)
}

/// Human-readable display name for one routine item, resolved against the
/// current store/launcher so the run summary names real things.
fn item_display_name(
    store: &AppStore,
    launcher: &LauncherState,
    item: &RoutineItem,
) -> String {
    match item.kind.as_str() {
        "account" => {
            let account_id = item.account_id.as_deref().unwrap_or("");
            if let Ok(web_app) = store.get(&item.app_id) {
                if let Some(account) = web_app.accounts.iter().find(|a| a.id == account_id) {
                    return format!("{} — {}", web_app.name, account.label);
                }
                return web_app.name.clone();
            }
            item.app_id.clone()
        }
        _ => {
            let program_id = item.program_id.as_deref().unwrap_or("");
            launcher
                .list()
                .into_iter()
                .find(|p| p.id == program_id)
                .map(|p| p.name)
                .unwrap_or_else(|| program_id.to_string())
        }
    }
}

/// Pure summary builder: plain-language "Opened X of Y" plus per-failure
/// reasons. Extracted for testing (no AppHandle/State needed).
fn summarize(results: &[(String, Result<(), String>)]) -> String {
    let total = results.len();
    let ok = results.iter().filter(|(_, r)| r.is_ok()).count();
    if total == 0 {
        return "Nothing to open.".to_string();
    }
    if ok == total {
        return if total == 1 {
            format!("Opened {}.", results[0].0)
        } else {
            format!("Opened all {total}.")
        };
    }
    let failures: Vec<String> = results
        .iter()
        .filter_map(|(name, r)| r.as_ref().err().map(|e| format!("{name} failed: {e}")))
        .collect();
    if ok == 0 {
        format!("Nothing opened. {}", failures.join(" "))
    } else {
        format!("Opened {ok} of {total}. {}", failures.join(" "))
    }
}

/// Split a monitor rectangle into N equal vertical columns ("pillars").
/// Remainder pixels go to the last column so there are no gaps. Returns
/// (x, y, width, height) in physical pixels per column. Pure math —
/// unit-tested; the caller converts sizes to logical pixels.
pub fn tile_columns(
    mon_x: i32,
    mon_y: i32,
    mon_w: u32,
    mon_h: u32,
    n: usize,
) -> Vec<(i32, i32, u32, u32)> {
    if n == 0 || mon_w == 0 || mon_h == 0 {
        return Vec::new();
    }
    let n64 = n as u64;
    let w64 = mon_w as u64;
    (0..n)
        .map(|i| {
            let i64 = i as u64;
            let x0 = mon_x + (w64 * i64 / n64) as i32;
            let x1 = mon_x + (w64 * (i64 + 1) / n64) as i32;
            (x0, mon_y, (x1 - x0).max(0) as u32, mon_h)
        })
        .collect()
}

/// Run a routine: open every item in order, staggered ~250ms so windows and
/// processes don't all pile up at once. Account items reuse
/// `windows::open_account` (which enforces the LRU window cap); program
/// items reuse the launcher's server-side launch path. Returns a
/// plain-language summary — individual failures never abort the run.
///
/// MUST stay `async fn`: `open_account` builds OS windows, and window
/// creation inside a synchronous command deadlocks WebView2 (wry#583).
///
/// JS: `invoke("run_routine", { routineId })`.
#[tauri::command]
pub async fn run_routine(
    app: AppHandle,
    store: State<'_, AppStore>,
    adblock: State<'_, AdblockState>,
    winstate: State<'_, WindowState>,
    launcher: State<'_, LauncherState>,
    tabstate: State<'_, crate::tabs::TabState>,
    routine_id: String,
) -> Result<String, String> {
    let routine = load(&app)
        .into_iter()
        .find(|r| r.id == routine_id)
        .ok_or_else(|| "That routine is gone.".to_string())?;
    if routine.items.is_empty() {
        return Err("This routine has no items to open.".to_string());
    }

    // v0.13.0 tabbed layout: all account items open as tabs in ONE new
    // tabbed window, in routine order. Program items still launch
    // natively. Invalid/unknown apps are dropped by open_tabbed_window's
    // store validation, same as the + picker.
    if routine.layout == RoutineLayout::Tabbed {
        let mut tabs: Vec<crate::tabs::TabEntry> = Vec::new();
        let mut results: Vec<(String, Result<(), String>)> =
            Vec::with_capacity(routine.items.len());
        for (i, item) in routine.items.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            let name = item_display_name(&store, &launcher, item);
            let outcome = match item.kind.as_str() {
                "account" => {
                    tabs.push(crate::tabs::TabEntry {
                        app_id: item.app_id.clone(),
                        account_id: item.account_id.clone().unwrap_or_default(),
                        last_url: None,
                    });
                    Ok(())
                }
                "program" => {
                    let program_id = item.program_id.as_deref().unwrap_or("");
                    launcher.launch(program_id)
                }
                other => Err(format!("Unknown routine item kind \"{other}\".")),
            };
            results.push((name, outcome));
        }
        if !tabs.is_empty() {
            let info = crate::tabs::open_tabbed_window(
                &app,
                &store,
                &adblock,
                &tabstate,
                crate::tabs::OpenTabbedParams {
                    initial: tabs,
                    active: 0,
                    placement: None,
                    restore_id: None,
                },
            )
            .map_err(|e| format!("couldn't open the tabbed window: {e}"))?;
            results.push((
                format!("{} tabs", info.tabs.len()),
                Ok(()),
            ));
        }
        return Ok(summarize(&results));
    }

    // v0.9.0 side-by-side layout: tile the routine's ACCOUNT windows as
    // equal columns across the cursor's monitor, left to right in routine
    // order. Program items launch natively and can't be positioned, so
    // they take no slot. Physical monitor geometry is converted to logical
    // pixels via the monitor's scale factor (HiDPI stays exact).
    let placements: Vec<crate::windows::WindowPlacement> =
        if routine.layout == RoutineLayout::SideBySide {
            match crate::windows::cursor_monitor(&app) {
                Some(monitor) => {
                    let account_count = routine
                        .items
                        .iter()
                        .filter(|i| i.kind == "account")
                        .count();
                    let mp = monitor.position();
                    let ms = monitor.size();
                    let scale = monitor.scale_factor().max(0.01);
                    tile_columns(mp.x, mp.y, ms.width, ms.height, account_count)
                        .into_iter()
                        .map(|(x, y, w, h)| crate::windows::WindowPlacement {
                            x: x as f64 / scale,
                            y: y as f64 / scale,
                            width: w as f64 / scale,
                            height: h as f64 / scale,
                        })
                        .collect()
                }
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };

    let mut results: Vec<(String, Result<(), String>)> = Vec::with_capacity(routine.items.len());
    let mut account_idx = 0usize;
    for (i, item) in routine.items.iter().enumerate() {
        if i > 0 {
            // Stagger opens so N windows/processes don't spawn in one burst.
            // (tokio "time" feature; Cargo.toml spec in INTEGRATION.md.)
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let name = item_display_name(&store, &launcher, item);
        let outcome = match item.kind.as_str() {
            "account" => {
                let account_id = item.account_id.as_deref().unwrap_or("");
                let placement = placements.get(account_idx).copied();
                account_idx += 1;
                crate::windows::open_account_placed(
                    &app, &store, &adblock, &winstate, &item.app_id, account_id, placement,
                )
            }
            "program" => {
                let program_id = item.program_id.as_deref().unwrap_or("");
                launcher.launch(program_id)
            }
            other => Err(format!("Unknown routine item kind \"{other}\".")),
        };
        results.push((name, outcome));
    }
    Ok(summarize(&results))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_routine() -> Routine {
        Routine {
            id: "routine-1".to_string(),
            name: "Morning".to_string(),
            hotkey: Some("Ctrl+Alt+M".to_string()),
            layout: RoutineLayout::Cascade,
            items: vec![
                RoutineItem {
                    kind: "account".to_string(),
                    app_id: "app-gmail".to_string(),
                    account_id: Some("acct-work".to_string()),
                    program_id: None,
                },
                RoutineItem {
                    kind: "program".to_string(),
                    app_id: String::new(),
                    account_id: None,
                    program_id: Some("prog-1".to_string()),
                },
            ],
        }
    }

    #[test]
    fn routine_json_round_trip_with_defaults() {
        // Old/corrupt records deserialize through #[serde(default)].
        let json = r#"{"id":"r1"}"#;
        let r: Routine = serde_json::from_str(json).unwrap();
        assert_eq!(r.id, "r1");
        assert_eq!(r.name, "");
        assert!(r.hotkey.is_none());
        assert!(r.items.is_empty());

        let full = sample_routine();
        let encoded = serde_json::to_string(&full).unwrap();
        let decoded: Routine = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.id, full.id);
        assert_eq!(decoded.name, full.name);
        assert_eq!(decoded.hotkey, full.hotkey);
        assert_eq!(decoded.items.len(), 2);
        assert_eq!(decoded.items[0].kind, "account");
        assert_eq!(decoded.items[0].account_id.as_deref(), Some("acct-work"));
        assert_eq!(decoded.items[1].program_id.as_deref(), Some("prog-1"));
    }

    #[test]
    fn binding_id_format() {
        assert_eq!(binding_id("routine-123"), "routine:routine-123");
    }

    #[test]
    fn upsert_replaces_or_appends() {
        let mut list = vec![sample_routine()];
        let mut updated = sample_routine();
        updated.name = "Evening".to_string();
        upsert(&mut list, updated);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Evening");

        let mut other = sample_routine();
        other.id = "routine-2".to_string();
        upsert(&mut list, other);
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn validate_rejects_bad_routines() {
        let mut r = sample_routine();
        r.name = "   ".to_string();
        assert!(validate(&r).is_err());

        let mut r = sample_routine();
        r.items.clear();
        assert!(validate(&r).is_err());

        let mut r = sample_routine();
        r.items[0].kind = "widget".to_string();
        assert!(validate(&r).is_err());

        let mut r = sample_routine();
        r.items[1].program_id = Some("  ".to_string());
        assert!(validate(&r).is_err());

        assert!(validate(&sample_routine()).is_ok());
    }

    #[test]
    fn summarize_cases() {
        let ok = |n: &str| (n.to_string(), Ok(()));
        let fail = |n: &str, e: &str| (n.to_string(), Err(e.to_string()));

        assert_eq!(summarize(&[]), "Nothing to open.");
        assert_eq!(summarize(&[ok("Gmail")]), "Opened Gmail.");
        assert_eq!(
            summarize(&[ok("Gmail"), ok("Muse")]),
            "Opened all 2."
        );
        let mixed = summarize(&[ok("Gmail"), fail("Muse", "Account not found."), ok("Dashboard")]);
        assert!(mixed.starts_with("Opened 2 of 3. "));
        assert!(mixed.contains("Muse failed: Account not found."));
        let all_fail = summarize(&[fail("Muse", "Account not found.")]);
        assert!(all_fail.starts_with("Nothing opened. "));
        assert!(all_fail.contains("Muse failed: Account not found."));
    }

    #[test]
    fn tile_columns_geometry() {
        // 3 pillars across 1920x1080: exact thirds, no gaps, no overlap.
        let cols = tile_columns(0, 0, 1920, 1080, 3);
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0], (0, 0, 640, 1080));
        assert_eq!(cols[1], (640, 0, 640, 1080));
        assert_eq!(cols[2], (1280, 0, 640, 1080));

        // Remainder pixels land in the last column (100/3 = 33+33+34).
        let cols = tile_columns(0, 0, 100, 50, 3);
        assert_eq!(cols[0].2 + cols[1].2 + cols[2].2, 100);
        assert_eq!(cols[2].2, 34);
        // Columns are contiguous.
        assert_eq!(cols[1].0, cols[0].0 + cols[0].2 as i32);
        assert_eq!(cols[2].0, cols[1].0 + cols[1].2 as i32);

        // Single window fills the monitor.
        assert_eq!(tile_columns(0, 0, 1920, 1080, 1), vec![(0, 0, 1920, 1080)]);

        // Negative monitor origin (multi-monitor X11) is preserved.
        let cols = tile_columns(-1920, 0, 1920, 1080, 2);
        assert_eq!(cols[0], (-1920, 0, 960, 1080));
        assert_eq!(cols[1], (-960, 0, 960, 1080));

        // Degenerate inputs yield nothing rather than panicking.
        assert!(tile_columns(0, 0, 1920, 1080, 0).is_empty());
        assert!(tile_columns(0, 0, 0, 1080, 3).is_empty());
    }

    #[test]
    fn layout_defaults_to_cascade() {
        // Routines saved before v0.9.0 (no layout key) keep cascade behavior.
        let r: Routine = serde_json::from_str(r#"{"id":"r1","name":"M"}"#).unwrap();
        assert_eq!(r.layout, RoutineLayout::Cascade);

        let json = serde_json::to_string(&Routine {
            layout: RoutineLayout::SideBySide,
            ..sample_routine()
        })
        .unwrap();
        let decoded: Routine = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.layout, RoutineLayout::SideBySide);
    }

    #[test]
    fn layout_tabbed_round_trip() {
        // v0.13.0: the tabbed layout survives a JSON round trip.
        let json = serde_json::to_string(&Routine {
            layout: RoutineLayout::Tabbed,
            ..sample_routine()
        })
        .unwrap();
        assert!(json.contains("\"layout\":\"tabbed\""));
        let decoded: Routine = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.layout, RoutineLayout::Tabbed);
    }
}
