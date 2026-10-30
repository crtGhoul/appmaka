//! Persisted search index snapshot (`<app_data>/search-index.json`, v0.18.0).
//!
//! Stores prebuilt fuzzy entries and program metadata so the launcher search
//! bar is warm immediately on startup before the background program scan finishes.

use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

fn index_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("search-index.json"))
}

#[tauri::command]
pub fn load_search_index(app: AppHandle) -> Option<String> {
    let path = index_path(&app).ok()?;
    fs::read_to_string(path).ok()
}

#[tauri::command]
pub fn save_search_index(app: AppHandle, contents: String) -> Result<(), String> {
    let path = index_path(&app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("could not create app data dir: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, contents.as_bytes())
        .map_err(|e| format!("could not write search index: {e}"))?;
    fs::rename(&tmp, &path)
        .map_err(|e| format!("could not save search index: {e}"))?;
    Ok(())
}
