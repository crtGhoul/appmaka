//! Manually-added launcher programs (e.g. an MSI install like QuicMic that
//! registers in Add/Remove Programs but drops no Start Menu/Desktop shortcut).
//!
//! Stored in `<app-data>/custom-programs.json` — a NEW file the scan never
//! touches, so rescans can't wipe user entries (unlike `programs.json`, which
//! is regenerated wholesale). `list_programs` merges these into the scan
//! results with `is_custom: true`; `LauncherState::launch` resolves them by id
//! through the same server-side lookup, so the frontend can never ask the
//! backend to run an arbitrary path.
//!
//! NOTE (coordinator integration):
//! - add `mod custom_programs;` to main.rs (this file is currently pulled in
//!   via `#[path]` from launcher.rs so it compiles without touching main.rs);
//! - register `add_custom_program`, `remove_custom_program`, and
//!   `pick_executable` in the `invoke_handler` list;
//! - add `tauri-plugin-dialog` to Cargo.toml for `pick_executable`
//!   (it calls `tauri_plugin_dialog::DialogExt` and will not compile until
//!   the dependency exists).

use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

/// A user-added program entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomProgram {
    /// `"custom-"` + stable hex hash of the lowercased exe path.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Absolute path to the executable (a real binary on Linux too, not a
    /// .desktop file — the Linux launch path spawns it directly).
    #[serde(default)]
    pub exe_path: String,
    /// Absolute path of the extracted icon PNG (Windows only; None on Linux).
    #[serde(default)]
    pub icon_path: Option<String>,
}

fn hash_str(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn programs_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("custom-programs.json"))
}

/// Load-or-empty: a missing or corrupt file is just "no custom programs".
pub fn load(app: &AppHandle) -> Vec<CustomProgram> {
    programs_path(app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        // v0.18.24: strip a leading UTF-8 BOM (Windows editors) before
        // parsing, so custom-programs.json is not silently dropped to empty.
        .and_then(|c| serde_json::from_str::<Vec<CustomProgram>>(c.trim_start_matches('\u{feff}')).ok())
        .map(|v| {
            v.into_iter()
                .filter(|p| !p.id.is_empty() && !p.exe_path.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn save(app: &AppHandle, list: &[CustomProgram]) -> Result<(), String> {
    let path = programs_path(app)?;
    let json =
        serde_json::to_string_pretty(list).map_err(|e| format!("could not encode: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| format!("could not write: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("could not save: {e}"))?;
    Ok(())
}

/// Add a program manually. JS: `invoke("add_custom_program", { name, exePath })`.
#[tauri::command]
pub fn add_custom_program(
    app: AppHandle,
    name: String,
    exe_path: String,
) -> Result<CustomProgram, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the program a name.".to_string());
    }
    if name.chars().count() > 80 {
        return Err("Name is too long (max 80 characters).".to_string());
    }
    let path = PathBuf::from(exe_path.trim());
    if !path.is_file() {
        return Err("That file doesn't exist.".to_string());
    }
    let exe_path = path.to_string_lossy().into_owned();
    let id = format!("custom-{}", hash_str(&exe_path.to_lowercase()));

    let mut list = load(&app);
    if let Some(existing) = list.iter().find(|p| p.id == id) {
        // Same exe re-added: refresh the name, keep the old icon.
        let mut updated = existing.clone();
        updated.name = name.to_string();
        if let Some(slot) = list.iter_mut().find(|p| p.id == id) {
            *slot = updated.clone();
        }
        save(&app, &list)?;
        return Ok(updated);
    }

    #[cfg(windows)]
    let icon_path = crate::launcher::extract_icon_png(&path, &icons_dir(&app)?);
    #[cfg(not(windows))]
    let icon_path: Option<String> = None;

    let program = CustomProgram {
        id,
        name: name.to_string(),
        exe_path,
        icon_path,
    };
    list.push(program.clone());
    save(&app, &list)?;
    Ok(program)
}

/// Remove a manually-added program. JS: `invoke("remove_custom_program", { programId })`.
#[tauri::command]
pub fn remove_custom_program(
    app: AppHandle,
    program_id: String,
) -> Result<(), String> {
    let mut list = load(&app);
    let before = list.len();
    list.retain(|p| p.id != program_id);
    if list.len() == before {
        return Err(format!(
            "No custom program with id \"{program_id}\" — only manually-added programs can be removed."
        ));
    }
    save(&app, &list)
}

/// Let the user pick an executable with the native file dialog. MUST stay
/// `async`: the blocking picker call runs on a blocking thread so the async
/// runtime (and the UI) is never stalled. Returns `Ok(None)` on cancel.
///
/// Requires `tauri-plugin-dialog` in Cargo.toml (coordinator adds it during
/// integration) — without it this does not compile, which is expected.
#[tauri::command]
pub async fn pick_executable(app: AppHandle) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use tauri_plugin_dialog::DialogExt;
        let picked = app
            .dialog()
            .file()
            .add_filter("Executables", &["exe"])
            .blocking_pick_file();
        Ok::<_, String>(picked.map(|fp| fp.to_string()))
    })
    .await
    .map_err(|e| format!("file picker failed: {e}"))?
}

#[cfg(windows)]
fn icons_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("program-icons");
    fs::create_dir_all(&dir).map_err(|e| format!("could not create program icon dir: {e}"))?;
    Ok(dir)
}
