//! One-time retirement of the removed workspaces feature.
//!
//! Workspaces (named groups of apps, `workspaces.json`) were removed; the
//! launcher is a flat app grid now. This runs once at startup, before the
//! launcher settings load and the hotkey registry, and is best-effort
//! throughout: any failure leaves the files alone and startup continues.
//!
//! What it does, in order:
//! 1. Moves `<app-data>/workspaces.json` to `workspaces.json.bak`. The file
//!    is never deleted — a user who wants their old groups back still has
//!    them. No parsing: even a corrupt file is preserved, not dropped.
//! 2. Strips dangling `workspace:<id>` references from `launcher.json`
//!    (pinned tiles, dock-profile pins, usage keys). Apps themselves were
//!    never nested inside workspaces, so nothing needs flattening.
//!
//! Hotkey bindings need no separate cleanup: bindings only register from
//! `workspaces.json` at startup, and the file is already moved by the
//! time the registry reads. Idempotent — a second run finds nothing to
//! do and writes nothing.

use std::fs;
use std::path::Path;
use tauri::{AppHandle, Manager};

use crate::launcher_settings::{self, LauncherSettings};

const WORKSPACE_TAG_PREFIX: &str = "workspace:";

/// Move a legacy `workspaces.json` aside and clean its references out of
/// the launcher settings. Never panics, never fails startup.
pub fn retire_legacy_workspaces(app: &AppHandle) {
    let Some(dir) = app
        .path()
        .app_data_dir()
        .map_err(|e| eprintln!("workspace migration: {e}"))
        .ok()
    else {
        return;
    };
    retire_in_dir(&dir);
    let mut settings = launcher_settings::load(app);
    if strip_workspace_references(&mut settings) {
        if let Err(e) = launcher_settings::save(app, &settings) {
            eprintln!("workspace migration: could not save cleaned settings: {e}");
        }
    }
}

/// Move `workspaces.json` to `workspaces.json.bak` in `dir`, if present.
/// Returns true when a file was moved.
fn retire_in_dir(dir: &Path) -> bool {
    let path = dir.join("workspaces.json");
    if !path.exists() {
        return false;
    }
    let bak = dir.join("workspaces.json.bak");
    // Renaming onto an existing file fails on Windows, so clear a stale
    // backup first. The old backup is superseded by the newer file.
    let _ = fs::remove_file(&bak);
    match fs::rename(&path, &bak) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("workspace migration: could not move workspaces.json aside: {e}");
            false
        }
    }
}

/// Drop every `workspace:<id>` tag from a launcher-settings record.
/// Returns true when anything was removed.
fn strip_workspace_references(settings: &mut LauncherSettings) -> bool {
    let mut changed = false;
    let before = settings.pinned.len();
    settings
        .pinned
        .retain(|p| !p.starts_with(WORKSPACE_TAG_PREFIX));
    changed |= settings.pinned.len() != before;
    for profile in &mut settings.dock_profiles {
        let before = profile.pins.len();
        profile
            .pins
            .retain(|p| !p.starts_with(WORKSPACE_TAG_PREFIX));
        changed |= profile.pins.len() != before;
    }
    let before = settings.usage.len();
    settings
        .usage
        .retain(|k, _| !k.starts_with(WORKSPACE_TAG_PREFIX));
    changed |= settings.usage.len() != before;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher_settings::DockProfile;
    use serde::Deserialize;
    use std::path::PathBuf;

    /// Minimal shape of the legacy file: unknown fields must be ignored,
    /// never fail the parse (no deny_unknown_fields anywhere in the repo).
    #[derive(Debug, Deserialize)]
    struct LegacyWorkspaceFile {
        #[serde(default)]
        workspaces: Vec<LegacyWorkspace>,
        #[serde(default)]
        active_workspace_id: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    struct LegacyWorkspace {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        hotkey: Option<String>,
        #[serde(default)]
        members: Vec<LegacyMember>,
    }

    #[derive(Debug, Deserialize)]
    struct LegacyMember {
        #[serde(default)]
        app_id: String,
        #[serde(default)]
        account_id: Option<String>,
    }

    /// The realistic legacy fixture from the Phase 1 audit, plus an
    /// unknown extra field ("color") that must be ignored.
    const LEGACY_FIXTURE: &str = r#"{"workspaces":[{"id":"ws-1763421000000-0","name":"Work","hotkey":"Ctrl+Alt+W","color":"red","members":[{"app_id":"app-1763400000000-0","account_id":null},{"app_id":"app-1763400000000-1","account_id":"acct-1763400000000-3"}]},{"id":"ws-1763421000000-1","name":"Personal","hotkey":null,"members":[]}],"active_workspace_id":"ws-1763421000000-0"}"#;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "appmaka-ws-migration-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn legacy_file_moves_to_bak_and_unknown_fields_are_ignored() {
        let dir = temp_dir("bak");
        fs::write(dir.join("workspaces.json"), LEGACY_FIXTURE).unwrap();
        assert!(retire_in_dir(&dir));
        assert!(!dir.join("workspaces.json").exists());
        let bak = fs::read_to_string(dir.join("workspaces.json.bak")).unwrap();
        assert_eq!(bak, LEGACY_FIXTURE);
        // The preserved file still parses, unknown fields and all.
        let parsed: LegacyWorkspaceFile = serde_json::from_str(&bak).unwrap();
        assert_eq!(parsed.workspaces.len(), 2);
        assert_eq!(parsed.workspaces[0].id, "ws-1763421000000-0");
        assert_eq!(parsed.workspaces[0].name, "Work");
        assert_eq!(
            parsed.workspaces[0].hotkey.as_deref(),
            Some("Ctrl+Alt+W")
        );
        assert_eq!(parsed.workspaces[0].members.len(), 2);
        assert_eq!(
            parsed.workspaces[0].members[0].app_id,
            "app-1763400000000-0"
        );
        assert_eq!(
            parsed.workspaces[0].members[1].account_id.as_deref(),
            Some("acct-1763400000000-3")
        );
        assert_eq!(
            parsed.active_workspace_id.as_deref(),
            Some("ws-1763421000000-0")
        );
        // Second run is a no-op: nothing left to move, backup untouched.
        assert!(!retire_in_dir(&dir));
        assert_eq!(
            fs::read_to_string(dir.join("workspaces.json.bak")).unwrap(),
            LEGACY_FIXTURE
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_legacy_file_is_a_noop() {
        let dir = temp_dir("missing");
        assert!(!retire_in_dir(&dir));
        assert!(!dir.join("workspaces.json.bak").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_workspace_references_cleans_tags_and_keeps_apps() {
        let mut settings = LauncherSettings {
            pinned: vec![
                "workspace:ws-1763421000000-0".to_string(),
                "app-1763400000000-0".to_string(),
                "workspace:ws-1763421000000-1".to_string(),
            ],
            dock_profiles: vec![DockProfile {
                name: "Default".to_string(),
                edge: "bottom".to_string(),
                pins: vec![
                    "workspace:ws-1763421000000-0".to_string(),
                    "account:acct-1".to_string(),
                ],
            }],
            ..LauncherSettings::default()
        };
        settings.usage.insert(
            "workspace:ws-1763421000000-0".to_string(),
            Default::default(),
        );
        settings.usage.insert(
            "app-1763400000000-0".to_string(),
            Default::default(),
        );
        assert!(strip_workspace_references(&mut settings));
        assert_eq!(settings.pinned, vec!["app-1763400000000-0".to_string()]);
        assert_eq!(
            settings.dock_profiles[0].pins,
            vec!["account:acct-1".to_string()]
        );
        assert_eq!(settings.usage.len(), 1);
        assert!(settings.usage.contains_key("app-1763400000000-0"));
        // Clean a second time: nothing left to strip.
        assert!(!strip_workspace_references(&mut settings));
    }
}
