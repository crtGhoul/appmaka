//! Launcher settings: summon hotkey + run-at-startup, in one JSON file.
//!
//! The hotkey is stored as a string like `"Alt+Space"` and parsed by
//! tauri-plugin-global-shortcut when registered. When the user changes it,
//! the old binding is unregistered first; if the new one fails to register
//! (taken by another app, invalid), the old one is restored and the change is
//! reported as an error naming the hotkey — the launcher is never left with
//! no way to be summoned.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::GlobalShortcutExt;

pub const DEFAULT_HOTKEY: &str = "Alt+Space";

/// Default translucency of the phone-folder launcher panel (matches the
/// original hard-coded CSS value). The user can make it more solid in the
/// launcher settings; 1.0 is fully opaque.
pub const DEFAULT_PANEL_OPACITY: f32 = 0.55;
/// Hard floor so the panel can never become unreadably faint.
pub const MIN_PANEL_OPACITY: f32 = 0.3;

fn default_hotkey() -> String {
    DEFAULT_HOTKEY.to_string()
}

fn default_opacity() -> f32 {
    DEFAULT_PANEL_OPACITY
}

fn default_true() -> bool {
    true
}

/// Launch-usage statistics for one tile, used for frequency ranking in the
/// launcher. The key in `LauncherSettings::usage` is a tagged tile id
/// (`app:<id>`, `account:<id>`, `program:<id>`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStat {
    #[serde(default)]
    pub count: u64,
    /// Unix seconds of the most recent launch.
    #[serde(default)]
    pub last_used: i64,
}

/// Which monitor the launcher summon should appear on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MonitorMode {
    /// Open on the monitor that holds the cursor (default).
    #[default]
    Cursor,
    /// Open on the primary monitor.
    Primary,
}

/// What AppMaka does with the previous session at startup (v0.9.5):
/// the open account windows + search window saved to session.json.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StartupMode {
    /// Reopen last session's windows automatically (default).
    #[default]
    Restore,
    /// Ask once on the first launcher summon.
    Ask,
    /// Start with no windows.
    Fresh,
}

/// Cap on tracked usage entries; the least recently used entries are
/// evicted first so the map can never grow unbounded.
pub const MAX_USAGE_ENTRIES: usize = 500;

/// Clamp to the usable range. NaN (which serde could never produce, but a
/// hand-edited file might) falls back to the default.
pub fn clamp_opacity(v: f32) -> f32 {
    if !v.is_finite() {
        DEFAULT_PANEL_OPACITY
    } else {
        v.clamp(MIN_PANEL_OPACITY, 1.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherSettings {
    /// A hand-edited partial file may omit this; the loader's empty-string
    /// fallback keeps the summon key working either way.
    #[serde(default = "default_hotkey")]
    pub hotkey: String,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default = "default_opacity")]
    pub panel_opacity: f32,
    /// Pinned launcher tiles, tagged ids: `app:<id>`, `account:<id>`,
    /// `program:<id>`. Old files migrate to an empty vec.
    #[serde(default)]
    pub pinned: Vec<String>,
    /// Program ids hidden from the launcher. Old files migrate to an empty vec.
    #[serde(default)]
    pub hidden_programs: Vec<String>,
    /// Per-tile launch statistics for frequency ranking, keyed by tagged id.
    /// Old files migrate to an empty map.
    #[serde(default)]
    pub usage: HashMap<String, UsageStat>,
    /// Whether the first-run intro card has been dismissed.
    #[serde(default)]
    pub seen_intro: bool,
    /// Which monitor the summon opens on. Old files migrate to `Cursor`.
    #[serde(default)]
    pub monitor_mode: MonitorMode,
    /// Whether to check for updates automatically (default on).
    #[serde(default = "default_true")]
    pub auto_update_check: bool,
    /// Web-search engine for the launcher's `?query` command:
    /// "duckduckgo" (default) or "google". Old files migrate to DuckDuckGo.
    #[serde(default = "default_search_engine")]
    pub search_engine: String,
    /// Whether the library's "Hidden programs" list is collapsed.
    /// None = never touched: the UI defaults to collapsed whenever the list
    /// is non-empty. Old files migrate to None.
    #[serde(default)]
    pub hidden_section_collapsed: Option<bool>,
    /// What to do with the previous session at startup (v0.9.5).
    /// Old files migrate to Restore.
    #[serde(default)]
    pub startup_mode: StartupMode,
    /// Custom pointer drawn inside app windows (v0.12.0): "off" (default,
    /// native cursor), "dot", "ring", or "trail". A page-drawn pointer stays
    /// visible even when the site or the OS cursor theme hides the native
    /// one. Old files migrate to "off".
    #[serde(default = "default_custom_cursor")]
    pub custom_cursor: String,
    /// Open apps/accounts as tabbed windows instead of plain page windows
    /// (v0.13.0). Old files migrate to false (plain windows).
    #[serde(default)]
    pub open_as_tabbed: bool,
}

fn default_custom_cursor() -> String {
    "off".to_string()
}

fn default_search_engine() -> String {
    "duckduckgo".to_string()
}

impl Default for LauncherSettings {
    fn default() -> Self {
        Self {
            hotkey: DEFAULT_HOTKEY.to_string(),
            autostart: false,
            panel_opacity: DEFAULT_PANEL_OPACITY,
            pinned: Vec::new(),
            hidden_programs: Vec::new(),
            usage: HashMap::new(),
            seen_intro: false,
            monitor_mode: MonitorMode::default(),
            auto_update_check: true,
            search_engine: default_search_engine(),
            hidden_section_collapsed: None,
            startup_mode: StartupMode::default(),
            custom_cursor: default_custom_cursor(),
            open_as_tabbed: false,
        }
    }
}

impl LauncherSettings {
    /// Pin or unpin a tagged tile id (`app:<id>`, `account:<id>`,
    /// `program:<id>` — v0.9.3's `search:<url-encoded query>` for pinned web
    /// searches, v0.9.6's `workspace:<id>` for pinned workspaces). Any string
    /// tag round-trips; the UI only offers tags it can render.
    pub fn toggle_pin(&mut self, item_id: &str) {
        if let Some(pos) = self.pinned.iter().position(|p| p == item_id) {
            self.pinned.remove(pos);
        } else {
            self.pinned.push(item_id.to_string());
        }
    }

    /// Hide or unhide a program tile by program id.
    pub fn set_program_hidden(&mut self, program_id: &str, hidden: bool) {
        if hidden {
            if !self.hidden_programs.iter().any(|h| h == program_id) {
                self.hidden_programs.push(program_id.to_string());
            }
        } else {
            self.hidden_programs.retain(|h| h != program_id);
        }
    }

    /// Record a launch for usage-frequency ranking: bump the count and stamp
    /// `last_used` with the current Unix time. The map is capped at
    /// `MAX_USAGE_ENTRIES`; the least recently used entries are evicted first.
    pub fn record_launch(&mut self, item_id: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let stat = self.usage.entry(item_id.to_string()).or_default();
        stat.count = stat.count.saturating_add(1);
        stat.last_used = now;
        while self.usage.len() > MAX_USAGE_ENTRIES {
            let oldest = self
                .usage
                .iter()
                .min_by_key(|(_, s)| (s.last_used, s.count))
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.usage.remove(&k);
                }
                None => break,
            }
        }
    }
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("launcher.json"))
}

pub fn load(app: &AppHandle) -> LauncherSettings {
    let merged = || -> Option<LauncherSettings> {
        let path = settings_path(app).ok()?;
        let raw = fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    };
    let mut s = merged().unwrap_or_default();
    if s.hotkey.trim().is_empty() {
        s.hotkey = DEFAULT_HOTKEY.to_string();
    }
    s.panel_opacity = clamp_opacity(s.panel_opacity);
    s
}

pub fn save(app: &AppHandle, s: &LauncherSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("could not create settings dir: {e}"))?;
    }
    let raw = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, raw).map_err(|e| format!("could not write launcher settings: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("could not save launcher settings: {e}"))?;
    Ok(())
}

/// Register `hotkey` as the summon shortcut. Best-effort at startup: if it
/// fails (already taken, invalid), it is logged and the app keeps running.
pub fn register_hotkey(app: &AppHandle, hotkey: &str) -> Result<(), String> {
    app.global_shortcut()
        .register(hotkey)
        .map_err(|e| format!("could not register {hotkey}: {e}"))
}

/// Runtime-only snapshot of whether the saved summon hotkey is actually
/// registered with the OS. Kept as managed state — never written to
/// launcher.json — so the frontend can warn when startup registration
/// failed (e.g. another app already owns Alt+Space).
#[derive(Debug, Clone, Default, Serialize)]
pub struct HotkeyStatus {
    /// The saved hotkey this status describes.
    pub hotkey: String,
    /// True when the OS accepted the registration.
    pub registered: bool,
    /// The registration failure reason, if any. Worded for direct display
    /// ("could not register Alt+Space: ...").
    pub error: Option<String>,
}

/// Swap the summon hotkey: unregister the old one, register the new one, and
/// roll back to the old one if the new registration fails.
pub fn set_hotkey(
    app: &AppHandle,
    settings: &mut LauncherSettings,
    new_hotkey: &str,
) -> Result<(), String> {
    let new_hotkey = new_hotkey.trim().to_string();
    if new_hotkey.is_empty() {
        return Err("Type a hotkey first, e.g. Alt+Space.".to_string());
    }
    if new_hotkey == settings.hotkey {
        return Ok(());
    }
    let old = settings.hotkey.clone();
    let _ = app.global_shortcut().unregister(old.as_str());
    if let Err(e) = app.global_shortcut().register(new_hotkey.as_str()) {
        // Roll back: never leave the user without a working summon key.
        // If the restore itself fails, say so loudly — the caller turns
        // this into a "no hotkey registered" status instead of pretending
        // the old key is fine.
        if let Err(rb) = app.global_shortcut().register(old.as_str()) {
            return Err(format!(
                "Couldn't use {new_hotkey} ({e}). Also couldn't restore {old} ({rb}) — no summon hotkey is registered right now."
            ));
        }
        return Err(format!(
            "Couldn't use {new_hotkey} ({e}). Kept {old}."
        ));
    }
    settings.hotkey = new_hotkey;
    save(app, settings)
}

/// Set the launcher panel translucency (0.3..=1.0). Saved immediately so
/// the choice survives restarts.
pub fn set_panel_opacity(
    app: &AppHandle,
    settings: &mut LauncherSettings,
    opacity: f32,
) -> Result<(), String> {
    settings.panel_opacity = clamp_opacity(opacity);
    save(app, settings)
}

/// Set the launcher's `?query` web-search engine ("duckduckgo" or "google").
/// JS: `invoke("set_search_engine", { engine })`.
#[tauri::command]
pub fn set_search_engine(
    app: AppHandle,
    engine: String,
) -> Result<LauncherSettings, String> {
    let engine = engine.trim().to_lowercase();
    if engine != "duckduckgo" && engine != "google" {
        return Err("Unknown search engine.".to_string());
    }
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.search_engine = engine;
    save(&app, &settings)?;
    Ok(settings.clone())
}

/// Set the custom page cursor ("off", "dot", "ring", "trail").
/// JS: `invoke("set_custom_cursor", { style })`. Takes effect on windows
/// opened after the change; already-open windows keep their cursor.
#[tauri::command]
pub fn set_custom_cursor(
    app: AppHandle,
    style: String,
) -> Result<LauncherSettings, String> {
    let style = style.trim().to_lowercase();
    if !["off", "dot", "ring", "trail"].contains(&style.as_str()) {
        return Err("Unknown cursor style.".to_string());
    }
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.custom_cursor = style;
    save(&app, &settings)?;
    Ok(settings.clone())
}

/// Set whether opening an app/account creates a tabbed window instead of
/// a plain page window (v0.13.0).
/// JS: `invoke("set_open_as_tabbed", { enabled })`.
#[tauri::command]
pub fn set_open_as_tabbed(
    app: AppHandle,
    enabled: bool,
) -> Result<LauncherSettings, String> {
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.open_as_tabbed = enabled;
    save(&app, &settings)?;
    Ok(settings.clone())
}

/// Collapse/expand the library's "Hidden programs" list. Saved immediately
/// so the choice survives restarts.
/// JS: `invoke("set_hidden_section_collapsed", { collapsed })`.
#[tauri::command]
pub fn set_hidden_section_collapsed(
    app: AppHandle,
    collapsed: bool,
) -> Result<LauncherSettings, String> {
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.hidden_section_collapsed = Some(collapsed);
    save(&app, &settings)?;
    Ok(settings.clone())
}

/// Toggle run-at-startup through the autostart plugin.
pub fn set_autostart(
    app: &AppHandle,
    settings: &mut LauncherSettings,
    enabled: bool,
) -> Result<(), String> {
    if enabled {
        app.autolaunch()
            .enable()
            .map_err(|e| format!("could not enable run at startup: {e}"))?;
    } else {
        app.autolaunch()
            .disable()
            .map_err(|e| format!("could not disable run at startup: {e}"))?;
    }
    settings.autostart = enabled;
    save(app, settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_object_falls_back_to_defaults() {
        let s: LauncherSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.hotkey, DEFAULT_HOTKEY);
        assert!(!s.autostart);
        assert_eq!(s.panel_opacity, DEFAULT_PANEL_OPACITY);
    }

    #[test]
    fn partial_file_keeps_known_fields() {
        let s: LauncherSettings =
            serde_json::from_str(r#"{"hotkey":"Ctrl+Alt+A","seen_intro":true}"#).unwrap();
        assert_eq!(s.hotkey, "Ctrl+Alt+A");
        assert!(!s.autostart);
        assert!(s.seen_intro);
    }

    #[test]
    fn toggle_pin_round_trips_workspace_tags() {
        let mut s = LauncherSettings::default();
        // v0.9.6: pinned workspaces ride the same tagged-id machinery.
        s.toggle_pin("workspace:abc123");
        assert_eq!(s.pinned, vec!["workspace:abc123".to_string()]);
        s.toggle_pin("workspace:abc123");
        assert!(s.pinned.is_empty());
        // Pin order is preserved across kinds.
        s.toggle_pin("app:abc");
        s.toggle_pin("workspace:abc123");
        s.toggle_pin("search:x");
        assert_eq!(
            s.pinned,
            vec![
                "app:abc".to_string(),
                "workspace:abc123".to_string(),
                "search:x".to_string()
            ]
        );
    }

    #[test]
    fn toggle_pin_round_trips_search_tags() {
        let mut s = LauncherSettings::default();
        // v0.9.3: pinned web searches ride the same tagged-id machinery.
        s.toggle_pin("search:weather%20houston");
        assert_eq!(s.pinned, vec!["search:weather%20houston".to_string()]);
        s.toggle_pin("search:weather%20houston");
        assert!(s.pinned.is_empty());
        // Other kinds are untouched by the same machinery.
        s.toggle_pin("app:abc");
        s.toggle_pin("search:x");
        assert_eq!(s.pinned.len(), 2);
    }

    #[test]
    fn startup_mode_defaults_to_restore() {
        // Old files without the field migrate to Restore (the default).
        let s: LauncherSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.startup_mode, StartupMode::Restore);
        assert_eq!(LauncherSettings::default().startup_mode, StartupMode::Restore);
    }

    #[test]
    fn startup_mode_parses_all_values() {
        for (raw, expected) in [
            ("restore", StartupMode::Restore),
            ("ask", StartupMode::Ask),
            ("fresh", StartupMode::Fresh),
        ] {
            let s: LauncherSettings =
                serde_json::from_str(&format!(r#"{{"startup_mode":"{raw}"}}"#)).unwrap();
            assert_eq!(s.startup_mode, expected);
        }
        // Serializes back lowercase for the frontend.
        let s = LauncherSettings {
            startup_mode: StartupMode::Ask,
            ..LauncherSettings::default()
        };
        let raw = serde_json::to_string(&s).unwrap();
        assert!(raw.contains("\"startup_mode\":\"ask\""));
    }
}
