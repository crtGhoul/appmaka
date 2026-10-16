//! Sleep labels + "Memory saved" (v0.15.0).
//!
//! Labels only — zero behavior change. When the idle watcher or the LRU
//! cap closes an account window (`CloseIntent::Background`), the account's
//! last attributed RAM is banked here so the launcher can show a
//! "Sleeping" chip on the tile and the Library header can total honest
//! "Memory saved" figures. Windows the USER closes are never recorded:
//! claiming savings for those would be invented numbers.
//!
//! Persisted to app-data `sleep_state.json` (serde defaults — a missing
//! or corrupt file is simply empty). Entries clear when the account's
//! window reopens.

use std::sync::Mutex;
use tauri::Manager;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SleepEntry {
    app_id: String,
    account_id: String,
    /// Last attributed RSS at close time, KiB.
    rss_kb: u64,
    closed_at: u64,
}

/// Live sleep bookkeeping, managed as Tauri state.
#[derive(Default)]
pub struct SleepState {
    inner: Mutex<Vec<SleepEntry>>,
}

/// One sleeping account, for the launcher.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SleepInfo {
    pub app_id: String,
    pub account_id: String,
    pub rss_kb: u64,
}

fn store_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("sleep_state.json"))
}

fn save(app: &tauri::AppHandle) {
    let entries: Vec<SleepEntry> = app
        .try_state::<SleepState>()
        .and_then(|s| s.inner.lock().ok().map(|v| v.clone()))
        .unwrap_or_default();
    if let Some(path) = store_path(app) {
        if let Ok(json) = serde_json::to_string_pretty(&entries) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// Bank an account's last attributed RAM as saved. Called only for
/// automatic reclaims (idle watcher / LRU cap) — never for user closes.
pub fn record_sleep(app: &tauri::AppHandle, app_id: &str, account_id: &str) {
    let rss_kb = crate::procattr::rss_for_key(app, app_id, account_id);
    if let Some(state) = app.try_state::<SleepState>() {
        if let Ok(mut entries) = state.inner.lock() {
            entries.retain(|e| !(e.app_id == app_id && e.account_id == account_id));
            entries.push(SleepEntry {
                app_id: app_id.to_string(),
                account_id: account_id.to_string(),
                rss_kb,
                closed_at: crate::windows::unix_secs(),
            });
            // Small and bounded: one entry per account, cap the file.
            entries.truncate(100);
        }
    }
    save(app);
}

/// The account is awake again — drop its saved figure.
pub fn clear_sleep(app: &tauri::AppHandle, app_id: &str, account_id: &str) {
    let mut changed = false;
    if let Some(state) = app.try_state::<SleepState>() {
        if let Ok(mut entries) = state.inner.lock() {
            let before = entries.len();
            entries.retain(|e| !(e.app_id == app_id && e.account_id == account_id));
            changed = entries.len() != before;
        }
    }
    if changed {
        save(app);
    }
}

/// Load persisted sleep entries at startup.
pub fn load(app: &tauri::AppHandle) -> SleepState {
    let entries: Vec<SleepEntry> = store_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default();
    SleepState {
        inner: Mutex::new(entries),
    }
}

/// Sleeping accounts for the launcher poll.
#[tauri::command]
pub fn sleep_state(app: tauri::AppHandle) -> Vec<SleepInfo> {
    app.try_state::<SleepState>()
        .and_then(|s| {
            s.inner.lock().ok().map(|entries| {
                entries
                    .iter()
                    .map(|e| SleepInfo {
                        app_id: e.app_id.clone(),
                        account_id: e.account_id.clone(),
                        rss_kb: e.rss_kb,
                    })
                    .collect()
            })
        })
        .unwrap_or_default()
}
