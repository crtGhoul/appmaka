//! Launcher extension commands: pin/hide tiles, launch-usage tracking,
//! monitor mode, first-run intro, and the auto-update-check preference.
//!
//! These are pure settings commands — no window creation. They follow the
//! same pattern as the core launcher commands in `main.rs`: lock the managed
//! `LauncherSettings` state, mutate it through the helpers in
//! `launcher_settings.rs`, persist with `launcher_settings::save`, and return
//! a clone so the UI paints immediately.
//!
//! Naming note: Tauri exposes Rust snake_case params as camelCase to JS, so
//! e.g. `item_id` is invoked as `invoke("toggle_pin", { itemId })`.

use std::sync::Mutex;
use tauri::{AppHandle, Manager};

use crate::launcher_settings::{self, LauncherSettings, MonitorMode};

/// Pin or unpin a launcher tile. `item_id` is a tagged id: `app:<id>`,
/// `account:<id>`, or `program:<id>`.
/// JS: `invoke("toggle_pin", { itemId })`
/// Returns the updated settings so the UI paints immediately.
#[tauri::command]
pub fn toggle_pin(app: AppHandle, item_id: String) -> Result<LauncherSettings, String> {
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.toggle_pin(&item_id);
    launcher_settings::save(&app, &settings)?;
    Ok(settings.clone())
}

/// Hide or unhide a program tile by program id.
/// JS: `invoke("set_program_hidden", { programId, hidden })`
/// Returns the updated settings so the UI paints immediately.
#[tauri::command]
pub fn set_program_hidden(
    app: AppHandle,
    program_id: String,
    hidden: bool,
) -> Result<LauncherSettings, String> {
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.set_program_hidden(&program_id, hidden);
    launcher_settings::save(&app, &settings)?;
    Ok(settings.clone())
}

/// Record a launch of a tagged tile id for usage-frequency ranking.
/// JS: `invoke("record_launch", { itemId })`
#[tauri::command]
pub fn record_launch(app: AppHandle, item_id: String) -> Result<(), String> {
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.record_launch(&item_id);
    launcher_settings::save(&app, &settings)
}

/// Set which monitor the launcher summon opens on: `"cursor"` or `"primary"`.
/// Anything else is rejected with a clear error.
/// JS: `invoke("set_monitor_mode", { mode })`
/// Returns the updated settings so the UI paints immediately.
#[tauri::command]
pub fn set_monitor_mode(app: AppHandle, mode: String) -> Result<LauncherSettings, String> {
    let monitor_mode = match mode.as_str() {
        "cursor" => MonitorMode::Cursor,
        "primary" => MonitorMode::Primary,
        other => {
            return Err(format!(
                "unknown monitor mode {other:?}: expected \"cursor\" or \"primary\""
            ))
        }
    };
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.monitor_mode = monitor_mode;
    launcher_settings::save(&app, &settings)?;
    Ok(settings.clone())
}

/// Mark the first-run intro card as seen (or unseen).
/// JS: `invoke("set_seen_intro", { seen })`
/// Returns the updated settings so the UI paints immediately.
#[tauri::command]
pub fn set_seen_intro(app: AppHandle, seen: bool) -> Result<LauncherSettings, String> {
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.seen_intro = seen;
    launcher_settings::save(&app, &settings)?;
    Ok(settings.clone())
}

/// Enable or disable the automatic update check.
/// JS: `invoke("set_auto_update_check", { enabled })`
/// Returns the updated settings so the UI paints immediately.
#[tauri::command]
pub fn set_auto_update_check(app: AppHandle, enabled: bool) -> Result<LauncherSettings, String> {
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.auto_update_check = enabled;
    launcher_settings::save(&app, &settings)?;
    Ok(settings.clone())
}

/// Enable or disable background update pre-downloading.
/// JS: `invoke("set_auto_update_download", { enabled })`
#[tauri::command]
pub fn set_auto_update_download(app: AppHandle, enabled: bool) -> Result<LauncherSettings, String> {
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.auto_update_download = enabled;
    launcher_settings::save(&app, &settings)?;
    Ok(settings.clone())
}

/// Record (or clear) a pending downloaded update version.
/// JS: `invoke("set_pending_update_version", { version })`
#[tauri::command]
pub fn set_pending_update_version(
    app: AppHandle,
    version: Option<String>,
) -> Result<LauncherSettings, String> {
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.pending_update_version = version.and_then(|v| {
        let trimmed = v.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });
    launcher_settings::save(&app, &settings)?;
    Ok(settings.clone())
}

