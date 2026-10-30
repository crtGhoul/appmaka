//! Centralized global-hotkey registry (v0.8.0).
//!
//! Named bindings for routines ("routine:<id>"), workspaces
//! ("workspace:<id>") and per-command hotkeys ("cmd:<id>") all live here,
//! so the global-shortcut handler in main.rs can dispatch a pressed shortcut
//! to the right action instead of blindly toggling the launcher window.
//!
//! Conflict detection is done on the shortcut *id* (the canonical
//! modifiers+key the parser produces), so "Ctrl+Alt+G" and "Alt+Ctrl+G"
//! are correctly treated as the same hotkey.
//!
//! Errors are worded for direct display in the UI (v0.6.1 style) — plain
//! language, naming the hotkey, never a debug dump.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};

/// What a named hotkey binding fires.
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "kind", content = "target", rename_all = "lowercase")]
pub enum HotkeyKind {
    Routine { routine_id: String },
    Workspace { workspace_id: String },
    Command {
        app_id: String,
        account_id: Option<String>,
    },
    /// The clipboard history popup (v0.9.0).
    Clipboard,
    /// Built-in dictation start/stop (v0.14.0).
    Dictation,
    /// Auto-hiding edge dock toggle (v0.18.0).
    Dock,
}

/// Friendly one-line description of a binding, used in conflict errors.
fn describe_binding(binding_id: &str, kind: &HotkeyKind) -> String {
    let _ = binding_id;
    match kind {
        HotkeyKind::Routine { routine_id } => format!("the \"{routine_id}\" routine"),
        HotkeyKind::Workspace { workspace_id } => {
            format!("the \"{workspace_id}\" workspace")
        }
        HotkeyKind::Command { app_id, account_id } => match account_id {
            Some(a) => format!("the hotkey for account \"{a}\" in app \"{app_id}\""),
            None => format!("the hotkey for app \"{app_id}\""),
        },
        HotkeyKind::Clipboard => "the clipboard history popup".to_string(),
        HotkeyKind::Dictation => "the dictation hotkey".to_string(),
        HotkeyKind::Dock => "the edge dock hotkey".to_string(),
    }
}

/// Runtime snapshot of whether one binding's hotkey is actually registered
/// with the OS. Kept in managed state — never persisted — mirroring the
/// v0.6.1 summon-hotkey status the frontend already knows how to render.
#[derive(Debug, Clone, Serialize)]
pub struct BindingStatus {
    pub hotkey: String,
    pub registered: bool,
    /// Worded for direct display ("…is already used by…"), v0.6.1 style.
    pub error: Option<String>,
}

/// One live registry entry. The hotkey *string* is kept (in addition to the
/// shortcut id) because the plugin's `unregister` only accepts a string /
/// Shortcut, not a bare id.
#[derive(Debug, Clone)]
struct RegisteredBinding {
    kind: HotkeyKind,
    hotkey: String,
    shortcut_id: u32,
}

/// Managed state: binding id -> live binding.
type BindingMap = HashMap<String, RegisteredBinding>;
/// Managed state: shortcut id -> binding id (reverse lookup for the
/// main.rs press handler).
type ReverseMap = HashMap<u32, String>;
/// Managed state: binding id -> last registration status.
type StatusMap = HashMap<String, BindingStatus>;

/// Initialize the managed registry state. Call once at startup, before
/// `register_all_saved`.
pub fn init_registry(app: &AppHandle) {
    app.manage(Mutex::new(BindingMap::new()));
    app.manage(Mutex::new(ReverseMap::new()));
    app.manage(Mutex::new(StatusMap::new()));
}

fn parse_hotkey(hotkey: &str) -> Result<Shortcut, String> {
    Shortcut::from_str(hotkey.trim()).map_err(|_| {
        format!("\"{hotkey}\" doesn't look like a hotkey. Try something like Ctrl+Alt+G.")
    })
}

/// Syntax-check a hotkey string against the global-shortcut parser without
/// registering anything. Used by the HotkeyCapture component for immediate
/// inline validation while the user is still editing — a combination the
/// parser rejects here would fail the same way at save time.
pub fn validate_hotkey_syntax(hotkey: &str) -> Result<(), String> {
    parse_hotkey(hotkey).map(|_| ())
}

/// The launcher summon hotkey, for naming conflicts against it. Read from
/// the already-managed launcher settings; absent -> no summon comparison.
fn summon_shortcut_id(app: &AppHandle) -> Option<u32> {
    let settings = app.try_state::<Mutex<crate::launcher_settings::LauncherSettings>>()?;
    let settings = settings.lock().ok()?;
    let id = Shortcut::from_str(settings.hotkey.trim()).ok()?.id();
    Some(id)
}

/// Set (or replace) a binding. An empty `hotkey` removes the binding.
///
/// Conflict rules, in order:
/// 1. Same binding id + same hotkey -> no-op success (idempotent).
/// 2. A *different* binding already uses this hotkey -> plain-language
///    error naming the other binding.
/// 3. The hotkey is registered by something else in this app (e.g. the
///    launcher summon hotkey) -> error naming it.
///
/// Registration failure (taken by another app, invalid on this OS) ->
/// error naming the hotkey; the previous binding is restored when one
/// existed.
pub fn set_binding(
    app: &AppHandle,
    binding_id: &str,
    kind: HotkeyKind,
    hotkey: &str,
) -> Result<(), String> {
    let hotkey = hotkey.trim().to_string();
    if hotkey.is_empty() {
        return remove_binding(app, binding_id);
    }

    let shortcut = parse_hotkey(&hotkey)?;
    let id = shortcut.id();

    {
        let bindings = app
            .try_state::<Mutex<BindingMap>>()
            .ok_or("hotkey registry not initialized")?;
        let bindings = bindings
            .lock()
            .map_err(|e| format!("hotkey registry poisoned: {e}"))?;
        if let Some(existing) = bindings.get(binding_id) {
            if existing.shortcut_id == id {
                // Same binding, same hotkey: nothing to do.
                return Ok(());
            }
        }
        // Same hotkey claimed by a different binding.
        if let Some((other_id, other)) = bindings
            .iter()
            .find(|(bid, b)| bid.as_str() != binding_id && b.shortcut_id == id)
        {
            return Err(format!(
                "\"{hotkey}\" is already used by {}. Pick another one.",
                describe_binding(other_id, &other.kind)
            ));
        }
        // Registered with the OS by something outside this registry —
        // almost always the launcher summon hotkey (it goes through the
        // same plugin, so is_registered sees it).
        if app.global_shortcut().is_registered(hotkey.as_str()) {
            if summon_shortcut_id(app) == Some(id) {
                return Err(format!(
                    "\"{hotkey}\" is the launcher's summon hotkey. Pick another one."
                ));
            }
            return Err(format!(
                "\"{hotkey}\" is already used by another AppMaka shortcut. Pick another one."
            ));
        }
    }

    // Unregister the old hotkey for this binding, if any.
    let old_hotkey: Option<String> = {
        let bindings = app
            .try_state::<Mutex<BindingMap>>()
            .ok_or("hotkey registry not initialized")?;
        let bindings = bindings
            .lock()
            .map_err(|e| format!("hotkey registry poisoned: {e}"))?;
        bindings.get(binding_id).map(|b| b.hotkey.clone())
    };
    if let Some(old) = old_hotkey.as_deref() {
        if old != hotkey {
            let _ = app.global_shortcut().unregister(old);
        }
    }

    if let Err(e) = app.global_shortcut().register(hotkey.as_str()) {
        // Restore the previous binding so the user is never left with a
        // half-moved hotkey.
        if let Some(old) = old_hotkey.as_deref() {
            let _ = app.global_shortcut().register(old);
        }
        return Err(format!(
            "Couldn't register \"{hotkey}\" ({e}). It may be taken by another app."
        ));
    }

    // Insert the new binding, remembering the old one so we can clear its
    // stale reverse-lookup entry.
    let old: Option<RegisteredBinding> = {
        let bindings = app
            .try_state::<Mutex<BindingMap>>()
            .ok_or("hotkey registry not initialized")?;
        let mut bindings = bindings
            .lock()
            .map_err(|e| format!("hotkey registry poisoned: {e}"))?;
        bindings.insert(
            binding_id.to_string(),
            RegisteredBinding {
                kind,
                hotkey: hotkey.clone(),
                shortcut_id: id,
            },
        )
    };
    {
        let reverse = app
            .try_state::<Mutex<ReverseMap>>()
            .ok_or("hotkey registry not initialized")?;
        let mut reverse = reverse
            .lock()
            .map_err(|e| format!("hotkey registry poisoned: {e}"))?;
        if let Some(old) = old {
            reverse.remove(&old.shortcut_id);
        }
        reverse.insert(id, binding_id.to_string());
    }
    set_status(
        app,
        binding_id,
        BindingStatus {
            hotkey,
            registered: true,
            error: None,
        },
    );
    Ok(())
}

/// Remove a binding: unregister its hotkey and drop it from the registry.
/// Succeeds even when the binding doesn't exist.
pub fn remove_binding(app: &AppHandle, binding_id: &str) -> Result<(), String> {
    let old: Option<RegisteredBinding> = {
        let bindings = app
            .try_state::<Mutex<BindingMap>>()
            .ok_or("hotkey registry not initialized")?;
        let mut bindings = bindings
            .lock()
            .map_err(|e| format!("hotkey registry poisoned: {e}"))?;
        bindings.remove(binding_id)
    };
    if let Some(b) = old {
        let _ = app.global_shortcut().unregister(b.hotkey.as_str());
        if let Some(reverse) = app.try_state::<Mutex<ReverseMap>>() {
            if let Ok(mut reverse) = reverse.lock() {
                reverse.remove(&b.shortcut_id);
            }
        }
    }
    if let Some(statuses) = app.try_state::<Mutex<StatusMap>>() {
        if let Ok(mut statuses) = statuses.lock() {
            statuses.remove(binding_id);
        }
    }
    Ok(())
}

fn set_status(app: &AppHandle, binding_id: &str, status: BindingStatus) {
    if let Some(statuses) = app.try_state::<Mutex<StatusMap>>() {
        if let Ok(mut statuses) = statuses.lock() {
            statuses.insert(binding_id.to_string(), status);
        }
    }
}

/// Record a failed registration from outside this module (e.g. the
/// clipboard module's startup registration), so the UI can warn about it
/// the same way it warns about failed routine/workspace bindings.
pub fn record_binding_failure(
    app: &AppHandle,
    binding_id: &str,
    hotkey: String,
    error: String,
) {
    set_status(
        app,
        binding_id,
        BindingStatus {
            hotkey,
            registered: false,
            error: Some(error),
        },
    );
}

/// Look up the binding behind a pressed shortcut id, for the main.rs
/// handler dispatch.
pub fn lookup_binding(app: &AppHandle, shortcut_id: u32) -> Option<(String, HotkeyKind)> {
    let reverse = app.try_state::<Mutex<ReverseMap>>()?;
    let reverse = reverse.lock().ok()?;
    let binding_id = reverse.get(&shortcut_id)?.clone();
    drop(reverse);
    let bindings = app.try_state::<Mutex<BindingMap>>()?;
    let bindings = bindings.lock().ok()?;
    let b = bindings.get(&binding_id)?;
    Some((binding_id, b.kind.clone()))
}

/// Runtime status of one binding for the UI (conflict warning banners).
pub fn binding_status(app: &AppHandle, binding_id: &str) -> Option<BindingStatus> {
    let statuses = app.try_state::<Mutex<StatusMap>>()?;
    let statuses = statuses.lock().ok()?;
    statuses.get(binding_id).cloned()
}

/// Payload of the "hotkey-fired" event: `{ binding_id, kind, target }`,
/// matching the frontend's `HotkeyFiredPayload` in useHotkeyDispatch.ts
/// exactly (kind is a plain string; target carries the ids).
#[derive(Debug, Clone, Serialize)]
struct HotkeyFiredPayload {
    binding_id: String,
    kind: &'static str,
    target: HotkeyTarget,
}

#[derive(Debug, Clone, Serialize)]
struct HotkeyTarget {
    #[serde(skip_serializing_if = "Option::is_none")]
    routine_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    app_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
}

/// Emit "hotkey-fired" for a pressed shortcut. Called from the main.rs
/// global-shortcut handler on `Pressed` when `lookup_binding` hits. The
/// frontend dispatches via existing invoke commands (`run_routine`,
/// `set_active_workspace`, `open_account` / program launch).
pub fn emit_hotkey_fired(app: &AppHandle, binding_id: &str, kind: &HotkeyKind) {
    let (kind_str, target) = match kind {
        HotkeyKind::Routine { routine_id } => (
            "routine",
            HotkeyTarget {
                routine_id: Some(routine_id.clone()),
                workspace_id: None,
                app_id: None,
                account_id: None,
            },
        ),
        HotkeyKind::Workspace { workspace_id } => (
            "workspace",
            HotkeyTarget {
                routine_id: None,
                workspace_id: Some(workspace_id.clone()),
                app_id: None,
                account_id: None,
            },
        ),
        HotkeyKind::Command { app_id, account_id } => (
            "command",
            HotkeyTarget {
                routine_id: None,
                workspace_id: None,
                app_id: Some(app_id.clone()),
                account_id: account_id.clone(),
            },
        ),
        HotkeyKind::Clipboard => (
            "clipboard",
            HotkeyTarget {
                routine_id: None,
                workspace_id: None,
                app_id: None,
                account_id: None,
            },
        ),
        // Dictation is dispatched in Rust (main.rs), like Clipboard;
        // this arm only keeps the match exhaustive.
        HotkeyKind::Dictation => (
            "dictation",
            HotkeyTarget {
                routine_id: None,
                workspace_id: None,
                app_id: None,
                account_id: None,
            },
        ),
        HotkeyKind::Dock => (
            "dock",
            HotkeyTarget {
                routine_id: None,
                workspace_id: None,
                app_id: None,
                account_id: None,
            },
        ),
    };
    let _ = app.emit(
        "hotkey-fired",
        HotkeyFiredPayload {
            binding_id: binding_id.to_string(),
            kind: kind_str,
            target,
        },
    );
}

// ---------------------------------------------------------------------------
// Startup: register every saved binding, best-effort.
// ---------------------------------------------------------------------------

/// One hotkey entry in routines.json / workspaces.json. Unknown fields are
/// ignored so sibling workers can evolve their records freely.
#[derive(Debug, Deserialize)]
struct SavedHotkeyEntry {
    id: String,
    #[serde(default)]
    hotkey: Option<String>,
}

fn cmdhotkeys_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("cmdhotkeys.json"))
}

fn data_file(app: &AppHandle, name: &str) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join(name))
}

/// Read `(id, hotkey)` pairs from a sibling JSON file. Accepts either a
/// JSON array of `{id, hotkey}` records or an object keyed by id whose
/// values carry `hotkey`. Missing/partial files yield nothing — startup
/// must never crash on a sibling's file.
fn load_saved_entries(app: &AppHandle, file: &str) -> Vec<(String, String)> {
    let path = match data_file(app, file) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    if let Ok(entries) = serde_json::from_str::<Vec<SavedHotkeyEntry>>(&raw) {
        for e in entries {
            if let Some(h) = e.hotkey {
                let h = h.trim().to_string();
                if !h.is_empty() {
                    out.push((e.id, h));
                }
            }
        }
        return out;
    }
    #[derive(Debug, Deserialize)]
    struct HotkeyOnly {
        #[serde(default)]
        hotkey: Option<String>,
    }
    if let Ok(map) = serde_json::from_str::<HashMap<String, HotkeyOnly>>(&raw) {
        for (id, rec) in map {
            if let Some(h) = rec.hotkey {
                let h = h.trim().to_string();
                if !h.is_empty() {
                    out.push((id, h));
                }
            }
        }
    }
    // Workspaces envelope: {"workspaces": [{id, hotkey, ...}], ...}.
    // (Unknown fields ignored; SavedHotkeyEntry already skips them.)
    #[derive(Debug, Deserialize)]
    struct WorkspaceEnvelope {
        #[serde(default)]
        workspaces: Vec<SavedHotkeyEntry>,
    }
    if let Ok(env) = serde_json::from_str::<WorkspaceEnvelope>(&raw) {
        for e in env.workspaces {
            if let Some(h) = e.hotkey {
                let h = h.trim().to_string();
                if !h.is_empty() {
                    out.push((e.id, h));
                }
            }
        }
    }
    out
}

/// Startup: register every saved routine / workspace / command hotkey.
/// Best-effort per binding — a failure is recorded in the binding status
/// (so the UI can warn) and logged, never fatal.
pub fn register_all_saved(app: &AppHandle) {
    for (id, hotkey) in load_saved_entries(app, "routines.json") {
        let binding_id = format!("routine:{id}");
        let kind = HotkeyKind::Routine {
            routine_id: id.clone(),
        };
        if let Err(e) = set_binding(app, &binding_id, kind, &hotkey) {
            eprintln!("hotkey registry: routine \"{id}\": {e}");
            set_status(
                app,
                &binding_id,
                BindingStatus {
                    hotkey,
                    registered: false,
                    error: Some(e),
                },
            );
        }
    }
    for (id, hotkey) in load_saved_entries(app, "workspaces.json") {
        let binding_id = format!("workspace:{id}");
        let kind = HotkeyKind::Workspace {
            workspace_id: id.clone(),
        };
        if let Err(e) = set_binding(app, &binding_id, kind, &hotkey) {
            eprintln!("hotkey registry: workspace \"{id}\": {e}");
            set_status(
                app,
                &binding_id,
                BindingStatus {
                    hotkey,
                    registered: false,
                    error: Some(e),
                },
            );
        }
    }
    for entry in load_cmdhotkeys(app).unwrap_or_default() {
        let hotkey = entry.hotkey.trim().to_string();
        if hotkey.is_empty() {
            continue;
        }
        let kind = HotkeyKind::Command {
            app_id: entry.app_id.clone(),
            account_id: entry.account_id.clone(),
        };
        if let Err(e) = set_binding(app, &entry.id, kind, &hotkey) {
            eprintln!("hotkey registry: command \"{}\": {e}", entry.id);
            set_status(
                app,
                &entry.id,
                BindingStatus {
                    hotkey,
                    registered: false,
                    error: Some(e),
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Per-command hotkeys: cmdhotkeys.json persistence + Tauri commands.
// ---------------------------------------------------------------------------

/// One per-command hotkey row in cmdhotkeys.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmdHotkey {
    /// Stable binding id: `cmd:<app_id>` or `cmd:<app_id>:<account_id>`.
    /// This is also the registry binding id.
    pub id: String,
    pub app_id: String,
    pub account_id: Option<String>,
    #[serde(default)]
    pub hotkey: String,
}

fn load_cmdhotkeys(app: &AppHandle) -> Result<Vec<CmdHotkey>, String> {
    let path = cmdhotkeys_path(app)?;
    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("could not read command hotkeys: {e}")),
    };
    serde_json::from_str(&raw).map_err(|e| format!("could not parse command hotkeys: {e}"))
}

fn save_cmdhotkeys(app: &AppHandle, rows: &[CmdHotkey]) -> Result<(), String> {
    let path = cmdhotkeys_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("could not create app data dir: {e}"))?;
    }
    let raw = serde_json::to_string_pretty(rows).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, raw).map_err(|e| format!("could not write command hotkeys: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("could not save command hotkeys: {e}"))?;
    Ok(())
}

fn sanitize_id_part(s: &str) -> String {
    s.trim().replace(':', "_")
}

/// Binding id for a command hotkey: `cmd:<app_id>` or
/// `cmd:<app_id>:<account_id>`.
pub fn cmd_binding_id(app_id: &str, account_id: Option<&str>) -> String {
    match account_id {
        Some(a) if !a.trim().is_empty() => {
            format!("cmd:{}:{}", sanitize_id_part(app_id), sanitize_id_part(a))
        }
        _ => format!("cmd:{}", sanitize_id_part(app_id)),
    }
}

/// List all saved per-command hotkeys.
/// JS: `invoke("list_cmdhotkeys")`.
#[tauri::command]
pub fn list_cmdhotkeys(app: AppHandle) -> Result<Vec<CmdHotkey>, String> {
    load_cmdhotkeys(&app)
}

/// Save a per-command hotkey (create or replace) and register it.
/// An empty `hotkey` deletes the binding.
/// JS: `invoke("save_cmdhotkey", { appId, accountId, hotkey })`.
#[tauri::command]
pub fn save_cmdhotkey(
    app: AppHandle,
    app_id: String,
    account_id: Option<String>,
    hotkey: String,
) -> Result<CmdHotkey, String> {
    let app_id = app_id.trim().to_string();
    if app_id.is_empty() {
        return Err("Pick an app first.".to_string());
    }
    let account_id = account_id
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty());
    let id = cmd_binding_id(&app_id, account_id.as_deref());
    let hotkey = hotkey.trim().to_string();

    let mut rows = load_cmdhotkeys(&app)?;
    if hotkey.is_empty() {
        rows.retain(|r| r.id != id);
        save_cmdhotkeys(&app, &rows)?;
        remove_binding(&app, &id)?;
        return Ok(CmdHotkey {
            id,
            app_id,
            account_id,
            hotkey: String::new(),
        });
    }

    let kind = HotkeyKind::Command {
        app_id: app_id.clone(),
        account_id: account_id.clone(),
    };
    // Register first so a conflict never lands in the file.
    set_binding(&app, &id, kind, &hotkey)?;

    let row = CmdHotkey {
        id: id.clone(),
        app_id,
        account_id,
        hotkey,
    };
    if let Some(existing) = rows.iter_mut().find(|r| r.id == id) {
        *existing = row.clone();
    } else {
        rows.push(row.clone());
    }
    save_cmdhotkeys(&app, &rows)?;
    Ok(row)
}

/// Delete a per-command hotkey and unregister it.
/// JS: `invoke("delete_cmdhotkey", { id })`.
#[tauri::command]
pub fn delete_cmdhotkey(app: AppHandle, id: String) -> Result<(), String> {
    let mut rows = load_cmdhotkeys(&app)?;
    rows.retain(|r| r.id != id);
    save_cmdhotkeys(&app, &rows)?;
    remove_binding(&app, &id)?;
    Ok(())
}

/// Runtime status of one binding for the UI's conflict warning.
/// JS: `invoke("get_binding_status", { bindingId })`.
#[tauri::command]
pub fn get_binding_status(
    app: AppHandle,
    binding_id: String,
) -> Result<Option<BindingStatus>, String> {
    Ok(binding_status(&app, &binding_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_id_is_modifier_order_independent() {
        // "Ctrl+Alt+G" and "Alt+Ctrl+G" must conflict: same parsed id.
        let a = Shortcut::from_str("Ctrl+Alt+G").unwrap().id();
        let b = Shortcut::from_str("Alt+Ctrl+G").unwrap().id();
        assert_eq!(a, b);
    }

    #[test]
    fn hotkey_id_distinguishes_different_keys() {
        let a = Shortcut::from_str("Ctrl+Alt+G").unwrap().id();
        let b = Shortcut::from_str("Ctrl+Alt+H").unwrap().id();
        assert_ne!(a, b);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_hotkey("not a hotkey").is_err());
        assert!(parse_hotkey("").is_err());
        assert!(parse_hotkey("Ctrl+").is_err());
    }

    #[test]
    fn parse_accepts_common_forms() {
        assert!(parse_hotkey("Alt+Space").is_ok());
        assert!(parse_hotkey("ctrl+alt+g").is_ok());
        assert!(parse_hotkey("  Ctrl+Shift+K  ").is_ok());
    }

    #[test]
    fn cmd_binding_id_shapes() {
        assert_eq!(cmd_binding_id("gmail", None), "cmd:gmail");
        assert_eq!(cmd_binding_id("gmail", Some("")), "cmd:gmail");
        assert_eq!(cmd_binding_id("gmail", Some("work")), "cmd:gmail:work");
        // Colons can't leak into the tagged id format.
        assert_eq!(cmd_binding_id("a:b", Some("c:d")), "cmd:a_b:c_d");
    }

    #[test]
    fn binding_descriptions_are_plain_language() {
        let d = describe_binding(
            "routine:r1",
            &HotkeyKind::Routine {
                routine_id: "Morning".to_string(),
            },
        );
        assert!(d.contains("Morning") && d.contains("routine"));
        let d = describe_binding(
            "cmd:gmail:work",
            &HotkeyKind::Command {
                app_id: "gmail".to_string(),
                account_id: Some("work".to_string()),
            },
        );
        assert!(d.contains("gmail") && d.contains("work"));
    }

    #[test]
    fn hotkey_fired_payload_matches_frontend_contract() {
        // The v0.8.0 integration fix: the payload is { binding_id, kind,
        // target } with kind as a plain string (useHotkeyDispatch.ts).
        let payload = HotkeyFiredPayload {
            binding_id: "cmd:gmail:work".to_string(),
            kind: "command",
            target: HotkeyTarget {
                routine_id: None,
                workspace_id: None,
                app_id: Some("gmail".to_string()),
                account_id: Some("work".to_string()),
            },
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["binding_id"], "cmd:gmail:work");
        assert_eq!(v["kind"], "command");
        assert_eq!(v["target"]["app_id"], "gmail");
        assert_eq!(v["target"]["account_id"], "work");

        let payload = HotkeyFiredPayload {
            binding_id: "routine:r1".to_string(),
            kind: "routine",
            target: HotkeyTarget {
                routine_id: Some("r1".to_string()),
                workspace_id: None,
                app_id: None,
                account_id: None,
            },
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["kind"], "routine");
        assert_eq!(v["target"]["routine_id"], "r1");
    }

    #[test]
    fn cmdhotkey_row_round_trips_with_defaults() {
        // Old/partial files: missing hotkey/account_id must not fail.
        let row: CmdHotkey =
            serde_json::from_str(r#"{"id":"cmd:gmail","app_id":"gmail"}"#).unwrap();
        assert_eq!(row.hotkey, "");
        assert_eq!(row.account_id, None);
    }

    #[test]
    fn saved_entry_tolerates_array_and_map_shapes() {
        // Array shape.
        let raw = r#"[{"id":"r1","hotkey":"Ctrl+Alt+1"},{"id":"r2"}]"#;
        let entries: Vec<SavedHotkeyEntry> = serde_json::from_str(raw).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].hotkey.as_deref(), Some("Ctrl+Alt+1"));
        assert_eq!(entries[1].hotkey, None);
        // Map shape fallback: key becomes the id.
        let raw = r#"{"w1":{"hotkey":"Ctrl+Alt+2"}}"#;
        let map: HashMap<String, serde_json::Value> = serde_json::from_str(raw).unwrap();
        assert!(map.contains_key("w1"));
    }
}
