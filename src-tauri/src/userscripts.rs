//! Tampermonkey-Lite user scripts (`<app_data>/userscripts/*.user.js`, v0.18.0).
//!
//! Scans `.user.js` (and `.js`) files in the app-data `userscripts` folder,
//! parses `@name`, `@match`, and `@run-at` from the `==UserScript==` block,
//! and injects enabled matching scripts into account and tabbed windows.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserScriptSummary {
    pub id: String,
    pub name: String,
    pub matches: Vec<String>,
    pub run_at: String,
    pub enabled: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DisabledState {
    #[serde(default)]
    disabled: HashSet<String>,
}

fn userscripts_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("userscripts");
    fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create userscripts dir: {e}"))?;
    Ok(dir)
}

fn state_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("userscripts-state.json"))
}

fn load_disabled(app: &AppHandle) -> HashSet<String> {
    state_path(app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str::<DisabledState>(&raw).ok())
        .map(|s| s.disabled)
        .unwrap_or_default()
}

fn save_disabled(app: &AppHandle, disabled: &HashSet<String>) -> Result<(), String> {
    let path = state_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("could not create app data dir: {e}"))?;
    }
    let raw = serde_json::to_string_pretty(&DisabledState {
        disabled: disabled.clone(),
    })
    .map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, raw).map_err(|e| format!("could not write userscript state: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("could not save userscript state: {e}"))?;
    Ok(())
}

fn parse_header(filename: &str, source: &str) -> (String, Vec<String>, String) {
    let mut name = filename
        .strip_suffix(".user.js")
        .or_else(|| filename.strip_suffix(".js"))
        .unwrap_or(filename)
        .to_string();
    let mut matches = Vec::new();
    let mut run_at = "document-end".to_string();
    let mut in_block = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.contains("==UserScript==") {
            in_block = true;
            continue;
        }
        if trimmed.contains("==/UserScript==") {
            break;
        }
        if !in_block {
            continue;
        }
        let body = trimmed.trim_start_matches("//").trim();
        if let Some(rest) = body.strip_prefix("@name") {
            let v = rest.trim();
            if !v.is_empty() {
                name = v.to_string();
            }
        } else if let Some(rest) = body.strip_prefix("@match") {
            let v = rest.trim();
            if !v.is_empty() {
                matches.push(v.to_string());
            }
        } else if let Some(rest) = body.strip_prefix("@run-at") {
            let v = rest.trim();
            if !v.is_empty() {
                run_at = v.to_string();
            }
        }
    }
    (name, matches, run_at)
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" || pattern == "<all_urls>" {
        return true;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern.eq_ignore_ascii_case(text);
    }
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !text[pos..].starts_with(part) {
                return false;
            }
            pos += part.len();
        } else if i == parts.len() - 1 {
            return text[pos..].ends_with(part);
        } else if let Some(found) = text[pos..].find(part) {
            pos += found + part.len();
        } else {
            return false;
        }
    }
    true
}

fn scan_scripts(dir: &Path, disabled: &HashSet<String>) -> Vec<(UserScriptSummary, String)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !fname.ends_with(".js") {
            continue;
        }
        let Ok(source) = fs::read_to_string(&path) else {
            continue;
        };
        let (name, matches, run_at) = parse_header(fname, &source);
        let id = fname.to_string();
        let enabled = !disabled.contains(&id);
        out.push((
            UserScriptSummary {
                id,
                name,
                matches,
                run_at,
                enabled,
            },
            source,
        ));
    }
    out.sort_by_key(|a| a.0.name.to_lowercase());
    out
}

pub fn user_scripts_init_scripts(app: &AppHandle, page_url: &str) -> Vec<String> {
    let Ok(dir) = userscripts_dir(app) else {
        return Vec::new();
    };
    let disabled = load_disabled(app);
    scan_scripts(&dir, &disabled)
        .into_iter()
        .filter(|(meta, _)| {
            meta.enabled
                && (meta.matches.is_empty()
                    || meta.matches.iter().any(|m| wildcard_match(m, page_url)))
        })
        .map(|(_, src)| format!("(function(){{try{{{src}\n}}catch(e){{}}}})();"))
        .collect()
}

#[tauri::command]
pub fn list_user_scripts(app: AppHandle) -> Result<Vec<UserScriptSummary>, String> {
    let dir = userscripts_dir(&app)?;
    let disabled = load_disabled(&app);
    Ok(scan_scripts(&dir, &disabled)
        .into_iter()
        .map(|(meta, _)| meta)
        .collect())
}

#[tauri::command]
pub fn reload_user_scripts(app: AppHandle) -> Result<Vec<UserScriptSummary>, String> {
    list_user_scripts(app)
}

#[tauri::command]
pub fn set_user_script_enabled(
    app: AppHandle,
    id: String,
    enabled: bool,
) -> Result<Vec<UserScriptSummary>, String> {
    let mut disabled = load_disabled(&app);
    if enabled {
        disabled.remove(&id);
    } else {
        disabled.insert(id);
    }
    save_disabled(&app, &disabled)?;
    list_user_scripts(app)
}

#[tauri::command]
pub fn get_userscripts_dir(app: AppHandle) -> Result<String, String> {
    let dir = userscripts_dir(&app)?;
    Ok(dir.to_string_lossy().into_owned())
}
