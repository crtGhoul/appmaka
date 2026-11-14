//! Workspaces: named groups of apps/accounts ("Work", "Personal").
//!
//! A workspace is filter-only: the launcher shows just the workspace's member
//! apps/accounts (plus launcher commands and installed programs, which are
//! not member-scoped), and a hotkey can switch the active workspace. There is
//! no nesting and no per-workspace settings.
//!
//! Persisted in `<app-data>/workspaces.json` as
//! `{ "workspaces": [...], "active_workspace_id": "ws-…" | null }`
//! (`null` = the "All" view). Written via tmp+rename like custom-programs.json.
//!
//! Hotkey contract (implemented by the hotkeys worker in `hotkeys.rs`):
//!   pub fn set_binding(app: &AppHandle, binding_id: &str, kind: HotkeyKind, hotkey: &str) -> Result<(), String>
//!   pub fn remove_binding(app: &AppHandle, binding_id: &str) -> Result<(), String>
//! with `binding_id = format!("workspace:{id}")` and
//! `HotkeyKind::Workspace { workspace_id: String }`.
//! `set_active_workspace` emits `appmaka:workspace-changed` with the new
//! `active_workspace_id` (`String | null`); the frontend re-reads
//! `list_workspaces` on it. The hotkey worker routes a workspace-hotkey press
//! through `set_active_workspace` so the event flows the same way.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::hotkeys::{remove_binding, set_binding, HotkeyKind};
use crate::store::AppStore;

/// One member of a workspace. `account_id: None` means the whole app (every
/// account it has now or gains later); `Some(id)` means just that account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceMember {
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub account_id: Option<String>,
}

/// A named group of apps/accounts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Optional global hotkey that switches to this workspace (e.g.
    /// `"Ctrl+Alt+W"`). Format/validation is owned by the hotkeys worker.
    #[serde(default)]
    pub hotkey: Option<String>,
    #[serde(default)]
    pub members: Vec<WorkspaceMember>,
}

/// What `list_workspaces` returns: every workspace plus which one is active
/// (`None` = the "All" view).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceList {
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub active_workspace_id: Option<String>,
}

/// On-disk shape of workspaces.json.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct WorkspaceFile {
    #[serde(default)]
    workspaces: Vec<Workspace>,
    #[serde(default)]
    active_workspace_id: Option<String>,
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Serializes read-modify-write cycles so two commands can't interleave saves.
static FILE_LOCK: Mutex<()> = Mutex::new(());

fn new_id() -> String {
    let n = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "ws-{}-{n}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    )
}

fn binding_id(workspace_id: &str) -> String {
    format!("workspace:{workspace_id}")
}

fn file_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("workspaces.json"))
}

/// Load-or-default: a missing or corrupt file is just "no workspaces".
fn load_file(app: &AppHandle) -> WorkspaceFile {
    file_path(app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        // v0.18.24: strip a leading UTF-8 BOM (Windows editors) before
        // parsing, so workspaces.json is not silently dropped to empty.
        .and_then(|c| serde_json::from_str::<WorkspaceFile>(c.trim_start_matches('\u{feff}')).ok())
        .map(|f| WorkspaceFile {
            workspaces: f
                .workspaces
                .into_iter()
                .filter(|w| !w.id.is_empty() && !w.name.trim().is_empty())
                .collect(),
            active_workspace_id: f.active_workspace_id,
        })
        .unwrap_or_default()
}

fn save_file(app: &AppHandle, file: &WorkspaceFile) -> Result<(), String> {
    let path = file_path(app)?;
    let json =
        serde_json::to_string_pretty(file).map_err(|e| format!("could not encode: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| format!("could not write: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("could not save: {e}"))?;
    Ok(())
}

fn lock_file() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    FILE_LOCK.lock().map_err(|_| "workspace store busy".to_string())
}

/// Normalize an optional hotkey: blank/whitespace-only counts as "no hotkey".
fn clean_hotkey(hotkey: Option<String>) -> Option<String> {
    hotkey.and_then(|h| {
        let h = h.trim().to_string();
        if h.is_empty() {
            None
        } else {
            Some(h)
        }
    })
}

/// Does this member cover the given (app, account) tile? A whole-app member
/// (account_id None) covers every account of the app, present and future.
///
/// The launcher mirrors this predicate in TypeScript (`workspaceCovers` in
/// WorkspacesSection.tsx); the allow goes away if the backend ever filters
/// server-side.
#[allow(dead_code)]
pub fn member_covers(member: &WorkspaceMember, app_id: &str, account_id: &str) -> bool {
    // An empty app_id is malformed (validate_workspace drops such members);
    // it covers nothing, not even an empty query.
    if member.app_id.is_empty() || member.app_id != app_id {
        return false;
    }
    match &member.account_id {
        None => true,
        Some(id) => id == account_id,
    }
}

/// True when any member of the workspace covers the (app, account) tile.
/// (Same interim note as `member_covers`.)
#[allow(dead_code)]
pub fn workspace_covers(members: &[WorkspaceMember], app_id: &str, account_id: &str) -> bool {
    members.iter().any(|m| member_covers(m, app_id, account_id))
}

/// Validate a workspace's fields and member list against the current store.
/// Drops members that point at deleted apps/accounts; whole-app members only
/// need the app to exist.
fn validate_workspace(store: &AppStore, workspace: &mut Workspace) -> Result<(), String> {
    let name = workspace.name.trim().to_string();
    if name.is_empty() {
        return Err("Give the workspace a name.".to_string());
    }
    if name.chars().count() > 60 {
        return Err("Name is too long (max 60 characters).".to_string());
    }
    workspace.name = name;

    let apps = store.list().map_err(|e| format!("could not read apps: {e}"))?;
    workspace.members.retain(|m| {
        if m.app_id.is_empty() {
            return false;
        }
        let Some(app) = apps.iter().find(|a| a.id == m.app_id) else {
            return false;
        };
        match &m.account_id {
            None => true,
            Some(acct) => app.accounts.iter().any(|a| &a.id == acct),
        }
    });
    // Deduplicate exact repeats (whole-app + its account both listed is fine —
    // the whole-app entry already covers it).
    workspace.members.dedup();
    Ok(())
}

/// Sync the global hotkey binding for a workspace after its record changed.
/// `old_hotkey` / `new_hotkey` are already cleaned (None = no hotkey).
fn sync_hotkey(
    app: &AppHandle,
    workspace_id: &str,
    old_hotkey: Option<&str>,
    new_hotkey: Option<&str>,
) -> Result<(), String> {
    let id = binding_id(workspace_id);
    match (old_hotkey, new_hotkey) {
        (old, Some(new)) if old != Some(new) => {
            set_binding(
                app,
                &id,
                HotkeyKind::Workspace {
                    workspace_id: workspace_id.to_string(),
                },
                new,
            )
            .map_err(|e| format!("Hotkey not saved: {e}"))
        }
        (Some(_), None) => {
            // Best-effort cleanup: the binding may never have been registered.
            let _ = remove_binding(app, &id);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// List every workspace plus the active one (`None` = All).
/// JS: `invoke("list_workspaces")`.
#[tauri::command]
pub fn list_workspaces(app: AppHandle) -> Result<WorkspaceList, String> {
    let _guard = lock_file()?;
    let file = load_file(&app);
    // The active id may dangle after manual file edits; fall back to All.
    let active_workspace_id = file
        .active_workspace_id
        .filter(|id| file.workspaces.iter().any(|w| &w.id == id));
    Ok(WorkspaceList {
        workspaces: file.workspaces,
        active_workspace_id,
    })
}

/// Create or update a workspace. An empty id means "create".
/// JS: `invoke("save_workspace", { workspace })`.
#[tauri::command]
pub fn save_workspace(
    app: AppHandle,
    store: State<'_, AppStore>,
    mut workspace: Workspace,
) -> Result<WorkspaceList, String> {
    validate_workspace(&store, &mut workspace)?;
    workspace.hotkey = clean_hotkey(workspace.hotkey);

    let _guard = lock_file()?;
    let mut file = load_file(&app);

    let old_hotkey: Option<String> = if workspace.id.is_empty() {
        workspace.id = new_id();
        None
    } else {
        file.workspaces
            .iter()
            .find(|w| w.id == workspace.id)
            .and_then(|w| clean_hotkey(w.hotkey.clone()))
    };

    // Register the hotkey BEFORE persisting, so a rejected hotkey fails the
    // whole save instead of leaving a workspace whose hotkey silently
    // doesn't work.
    sync_hotkey(
        &app,
        &workspace.id,
        old_hotkey.as_deref(),
        workspace.hotkey.as_deref(),
    )?;

    match file.workspaces.iter_mut().find(|w| w.id == workspace.id) {
        Some(slot) => *slot = workspace,
        None => file.workspaces.push(workspace),
    }
    // Keep the list stable and readable: alphabetical by name.
    file.workspaces.sort_by_key(|a| a.name.to_lowercase());
    save_file(&app, &file)?;

    let active_workspace_id = file
        .active_workspace_id
        .filter(|id| file.workspaces.iter().any(|w| &w.id == id));
    Ok(WorkspaceList {
        workspaces: file.workspaces,
        active_workspace_id,
    })
}

/// Delete a workspace and drop its hotkey binding. If it was active, the
/// launcher falls back to All. JS: `invoke("delete_workspace", { workspaceId })`.
#[tauri::command]
pub fn delete_workspace(app: AppHandle, workspace_id: String) -> Result<WorkspaceList, String> {
    let _guard = lock_file()?;
    let mut file = load_file(&app);
    let before = file.workspaces.len();
    file.workspaces.retain(|w| w.id != workspace_id);
    if file.workspaces.len() == before {
        return Err("That workspace is already gone.".to_string());
    }
    // Best-effort: the binding may never have been registered.
    let _ = remove_binding(&app, &binding_id(&workspace_id));
    if file.active_workspace_id.as_deref() == Some(workspace_id.as_str()) {
        file.active_workspace_id = None;
    }
    save_file(&app, &file)?;
    emit_changed(&app, file.active_workspace_id.clone());
    Ok(WorkspaceList {
        workspaces: file.workspaces,
        active_workspace_id: file.active_workspace_id,
    })
}

/// Switch the active workspace (`None` = All). The launcher re-reads
/// `list_workspaces` on the `appmaka:workspace-changed` event.
/// JS: `invoke("set_active_workspace", { workspaceId })`.
#[tauri::command]
pub fn set_active_workspace(
    app: AppHandle,
    workspace_id: Option<String>,
) -> Result<WorkspaceList, String> {
    let _guard = lock_file()?;
    let mut file = load_file(&app);
    if let Some(id) = &workspace_id {
        if !file.workspaces.iter().any(|w| &w.id == id) {
            return Err("That workspace no longer exists.".to_string());
        }
    }
    file.active_workspace_id = workspace_id;
    save_file(&app, &file)?;
    emit_changed(&app, file.active_workspace_id.clone());
    Ok(WorkspaceList {
        workspaces: file.workspaces,
        active_workspace_id: file.active_workspace_id,
    })
}

/// Snapshot of currently-open account windows, derived without touching
/// windows.rs: account window labels are `acct-{app_id}-{account_id}`
/// (see `windows::account_window_label`), matched against the store's
/// app/account ids. Sorted for a stable member order.
pub fn open_windows_snapshot(app: &AppHandle, store: &AppStore) -> Vec<WorkspaceMember> {
    let apps = store.list().unwrap_or_default();
    let mut members: Vec<WorkspaceMember> = Vec::new();
    for label in app.webview_windows().keys() {
        let Some(rest) = label.strip_prefix("acct-") else {
            continue;
        };
        // The trailing "-" in the prefix keeps "app-1" from matching "app-10".
        for a in &apps {
            let prefix = format!("{}-", a.id);
            if let Some(account_id) = rest.strip_prefix(&prefix) {
                if a.accounts.iter().any(|ac| ac.id == account_id) {
                    members.push(WorkspaceMember {
                        app_id: a.id.clone(),
                        account_id: Some(account_id.to_string()),
                    });
                }
                break;
            }
        }
    }
    members.sort_by(|a, b| {
        a.app_id
            .cmp(&b.app_id)
            .then(a.account_id.cmp(&b.account_id))
    });
    members
}

/// Build a workspace from the account windows that are open right now.
/// Errors when nothing is open (an empty workspace would be useless).
/// JS: `invoke("create_workspace_from_open", { name })`.
#[tauri::command]
pub fn create_workspace_from_open(
    app: AppHandle,
    store: State<'_, AppStore>,
    name: String,
) -> Result<WorkspaceList, String> {
    let members = open_windows_snapshot(&app, &store);
    if members.is_empty() {
        return Err("No account windows are open — open the ones you want grouped first.".to_string());
    }
    let workspace = Workspace {
        id: String::new(), // create
        name,
        hotkey: None,
        members,
    };
    // Reuse the create path (validation, id, sorting, persistence).
    save_workspace(app, store, workspace)
}

fn emit_changed(app: &AppHandle, active_workspace_id: Option<String>) {
    let _ = app.emit("appmaka:workspace-changed", active_workspace_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(app_id: &str, account_id: Option<&str>) -> WorkspaceMember {
        WorkspaceMember {
            app_id: app_id.to_string(),
            account_id: account_id.map(|s| s.to_string()),
        }
    }

    #[test]
    fn whole_app_member_covers_every_account() {
        let m = member("app-1", None);
        assert!(member_covers(&m, "app-1", "main"));
        assert!(member_covers(&m, "app-1", "work"));
        assert!(member_covers(&m, "app-1", "brand-new-account"));
    }

    #[test]
    fn account_member_covers_only_its_account() {
        let m = member("app-1", Some("work"));
        assert!(member_covers(&m, "app-1", "work"));
        assert!(!member_covers(&m, "app-1", "main"));
        assert!(!member_covers(&m, "app-1", "work2"));
    }

    #[test]
    fn member_of_another_app_covers_nothing() {
        let m = member("app-2", None);
        assert!(!member_covers(&m, "app-1", "main"));
        let m2 = member("app-2", Some("main"));
        assert!(!member_covers(&m2, "app-1", "main"));
    }

    #[test]
    fn empty_member_list_covers_nothing() {
        let members: Vec<WorkspaceMember> = vec![];
        assert!(!workspace_covers(&members, "app-1", "main"));
    }

    #[test]
    fn mixed_members_cover_union() {
        let members = vec![member("app-1", Some("work")), member("app-2", None)];
        assert!(workspace_covers(&members, "app-1", "work"));
        assert!(!workspace_covers(&members, "app-1", "main"));
        assert!(workspace_covers(&members, "app-2", "anything"));
        assert!(!workspace_covers(&members, "app-3", "main"));
    }

    #[test]
    fn empty_app_id_never_covers() {
        let m = member("", None);
        assert!(!member_covers(&m, "app-1", "main"));
        assert!(!member_covers(&m, "", "main"));
    }

    #[test]
    fn clean_hotkey_treats_blank_as_none() {
        assert_eq!(clean_hotkey(None), None);
        assert_eq!(clean_hotkey(Some("".to_string())), None);
        assert_eq!(clean_hotkey(Some("   ".to_string())), None);
        assert_eq!(
            clean_hotkey(Some("  Ctrl+Alt+W  ".to_string())),
            Some("Ctrl+Alt+W".to_string())
        );
    }

    #[test]
    fn binding_id_namespaces_by_workspace() {
        assert_eq!(binding_id("ws-1"), "workspace:ws-1");
    }

    #[test]
    fn corrupt_file_loads_as_default() {
        // WorkspaceFile must tolerate garbage — mirrors the load-or-default
        // contract of load_file.
        let parsed: Result<WorkspaceFile, _> = serde_json::from_str("not json{");
        assert!(parsed.is_err());
        let empty = WorkspaceFile::default();
        assert!(empty.workspaces.is_empty());
        assert_eq!(empty.active_workspace_id, None);
    }
}
