//! Workspace dock settings and hotkey bindings (v0.17.0).

use std::sync::Mutex;
use tauri::{AppHandle, Manager};

use crate::hotkeys::{self, HotkeyKind};
use crate::launcher_settings::{self, DockProfile, HotkeyStatus, LauncherSettings};

const DOCK_BINDING_ID: &str = "dock:toggle";

pub fn toggle(app: &AppHandle) {
    let enabled = app
        .try_state::<Mutex<LauncherSettings>>()
        .and_then(|s| s.lock().ok().map(|mut st| {
            st.dock_enabled = !st.dock_enabled;
            let _ = launcher_settings::save(app, &st);
            st.dock_enabled
        }))
        .unwrap_or(false);
    let _ = enabled;
    apply_visibility(app);
}

pub fn register_saved_hotkey(app: &AppHandle) {
    let hk = app
        .try_state::<Mutex<LauncherSettings>>()
        .and_then(|s| s.lock().ok().map(|st| st.dock_hotkey.clone()))
        .unwrap_or_default();
    if !hk.trim().is_empty() {
        let _ = hotkeys::set_binding(app, DOCK_BINDING_ID, HotkeyKind::Dock, &hk);
    }
}

pub fn apply_visibility(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("dock") {
        let enabled = app
            .try_state::<Mutex<LauncherSettings>>()
            .and_then(|s| s.lock().ok().map(|st| st.dock_enabled))
            .unwrap_or(false);
        if enabled {
            crate::windows::ensure_on_visible_monitor(app, &w);
        } else {
            let _ = w.hide();
        }
    }
}

#[tauri::command]
pub fn set_dock_enabled(app: AppHandle, enabled: bool) -> Result<LauncherSettings, String> {
    let out = {
        let state = app.state::<Mutex<LauncherSettings>>();
        let mut settings = state
            .lock()
            .map_err(|e| format!("settings state poisoned: {e}"))?;
        settings.dock_enabled = enabled;
        launcher_settings::save(&app, &settings)?;
        settings.clone()
    };
    apply_visibility(&app);
    Ok(out)
}

#[tauri::command]
pub fn set_dock_edge(app: AppHandle, edge: String) -> Result<LauncherSettings, String> {
    let edge = edge.trim().to_lowercase();
    if !["bottom", "top", "left", "right"].contains(&edge.as_str()) {
        return Err("Unknown dock edge.".to_string());
    }
    let out = {
        let state = app.state::<Mutex<LauncherSettings>>();
        let mut settings = state
            .lock()
            .map_err(|e| format!("settings state poisoned: {e}"))?;
        settings.dock_edge = edge;
        launcher_settings::save(&app, &settings)?;
        settings.clone()
    };
    apply_visibility(&app);
    Ok(out)
}

#[tauri::command]
pub fn set_dock_hotkey(app: AppHandle, hotkey: String) -> Result<HotkeyStatus, String> {
    let trimmed = hotkey.trim().to_string();
    let res = hotkeys::set_binding(&app, DOCK_BINDING_ID, HotkeyKind::Dock, &trimmed);
    match res {
        Ok(()) => {
            let state = app.state::<Mutex<LauncherSettings>>();
            if let Ok(mut settings) = state.lock() {
                settings.dock_hotkey = trimmed.clone();
                let _ = launcher_settings::save(&app, &settings);
            }
            Ok(HotkeyStatus {
                hotkey: trimmed.clone(),
                registered: !trimmed.is_empty(),
                error: None,
            })
        }
        Err(err) => Ok(HotkeyStatus {
            hotkey: trimmed,
            registered: false,
            error: Some(err),
        }),
    }
}

#[tauri::command]
pub fn dock_hotkey_status(app: AppHandle) -> Result<HotkeyStatus, String> {
    let hk = app
        .state::<Mutex<LauncherSettings>>()
        .lock()
        .map(|s| s.dock_hotkey.clone())
        .unwrap_or_default();
    if let Some(st) = hotkeys::binding_status(&app, DOCK_BINDING_ID) {
        return Ok(HotkeyStatus {
            hotkey: st.hotkey,
            registered: st.registered,
            error: st.error,
        });
    }
    Ok(HotkeyStatus {
        hotkey: hk,
        registered: false,
        error: None,
    })
}

#[tauri::command]
pub fn save_dock_profile(app: AppHandle, name: String) -> Result<LauncherSettings, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Enter a profile name.".to_string());
    }
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    let profile = DockProfile {
        name: name.clone(),
        edge: settings.dock_edge.clone(),
        pins: settings.pinned.clone(),
    };
    settings.dock_profiles.retain(|p| p.name != name);
    settings.dock_profiles.push(profile);
    settings.dock_active_profile = name;
    launcher_settings::save(&app, &settings)?;
    Ok(settings.clone())
}

#[tauri::command]
pub fn delete_dock_profile(app: AppHandle, name: String) -> Result<LauncherSettings, String> {
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.dock_profiles.retain(|p| p.name != name);
    if settings.dock_active_profile == name {
        settings.dock_active_profile.clear();
    }
    launcher_settings::save(&app, &settings)?;
    Ok(settings.clone())
}

#[tauri::command]
pub fn apply_dock_profile(app: AppHandle, name: String) -> Result<LauncherSettings, String> {
    let out = {
        let state = app.state::<Mutex<LauncherSettings>>();
        let mut settings = state
            .lock()
            .map_err(|e| format!("settings state poisoned: {e}"))?;
        if let Some(found) = settings.dock_profiles.iter().find(|p| p.name == name).cloned() {
            settings.dock_edge = found.edge;
            settings.pinned = found.pins;
            settings.dock_active_profile = found.name;
            launcher_settings::save(&app, &settings)?;
        }
        settings.clone()
    };
    apply_visibility(&app);
    Ok(out)
}
