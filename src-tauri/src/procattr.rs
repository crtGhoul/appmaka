//! Per-account process attribution for the RAM tiles (v0.15.0).
//!
//! The dashboard's system-wide snapshot can't say which renderer belongs
//! to which account, so tiles used to show nothing per app. Attribution
//! works in two steps:
//!
//! 1. **Capture** — when an account window is built, snapshot the set of
//!    our descendant PIDs, then take a second look (bounded ~2s poll) for
//!    newly spawned direct children. A new browser process appears exactly
//!    once per account data directory (WebView2 creates one browser
//!    process per user-data-folder; WebKitGTK spawns its web/network
//!    processes per website-data-manager), so the diff belongs to the
//!    account whose window just opened. PIDs already claimed by another
//!    account are skipped (first claim wins — two windows opened at once
//!    must not double-count).
//! 2. **Query** — one memory-only process scan; for each account, sum the
//!    RSS of its attributed PIDs plus every process whose ancestry reaches
//!    one of them (renderers and GPU processes are descendants of the
//!    browser process, including ones spawned long after the capture).
//!
//! The main process (launcher shell) is deliberately unattributed — the
//! tile number is the account's page processes, and the dashboard keeps
//! the authoritative total. Nothing here changes any accounting; it only
//! slices the existing numbers per account.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use tauri::Manager;

/// Attribution key: one account's browser processes, shared by however
/// many windows that account currently has open.
pub fn account_key(app_id: &str, account_id: &str) -> String {
    format!("{app_id}:{account_id}")
}

/// Account key → attributed browser-process PIDs. Managed as Tauri state.
#[derive(Default)]
pub struct ProcessAttribution {
    inner: Mutex<HashMap<String, HashSet<u32>>>,
}

/// One account's attributed RSS, for the tiles.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountMemory {
    pub app_id: String,
    pub account_id: String,
    /// Attributed browser + descendant processes, KiB.
    pub rss_kb: u64,
}

/// All our descendant PIDs right now (memory-only scan — the same cheap
/// refresh the dashboard snapshot uses).
pub fn descendant_pids() -> HashSet<u32> {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System};
    let sys = System::new_with_specifics(
        RefreshKind::nothing()
            .with_processes(ProcessRefreshKind::nothing().with_memory().without_tasks()),
    );
    let self_pid = std::process::id();
    let mut out = HashSet::new();
    for (pid, proc_) in sys.processes() {
        let p = pid.as_u32();
        if p == self_pid {
            continue;
        }
        let mut ancestor = proc_.parent();
        while let Some(a) = ancestor {
            if a.as_u32() == self_pid {
                out.insert(p);
                break;
            }
            ancestor = sys.process(a).and_then(|pp| pp.parent());
        }
    }
    out
}

/// After an account window is built, find the browser process(es) that
/// appeared for it and attribute them to the account. Runs on a detached
/// thread so the open command returns immediately; the poll is bounded
/// (~2s) and never repeats.
pub fn capture_new_processes(
    app: tauri::AppHandle,
    app_id: String,
    account_id: String,
    before: HashSet<u32>,
) {
    let _ = std::thread::Builder::new()
        .name(format!("appmaka-procattr-{app_id}"))
        .spawn(move || {
            let key = account_key(&app_id, &account_id);
            let mut found: HashSet<u32> = HashSet::new();
            // Bounded poll: the browser process spawns asynchronously after
            // the WebView2/WebKit environment is created.
            for i in 0..20 {
                let now = descendant_pids();
                for p in now.difference(&before) {
                    if !found.contains(p) {
                        found.insert(*p);
                    }
                }
                // Give stragglers (GPU, crashpad) a moment once we see the
                // first new process, then stop.
                if !found.is_empty() && i >= 3 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if found.is_empty() {
                return;
            }
            // The window may have closed while we polled; don't attribute
            // processes to an account with nothing open.
            let still_open = app
                .try_state::<crate::windows::WindowState>()
                .map(|ws| ws.has_account_window(&app_id, &account_id))
                .unwrap_or(false);
            if !still_open {
                return;
            }
            if let Some(attr) = app.try_state::<ProcessAttribution>() {
                if let Ok(mut map) = attr.inner.lock() {
                    // First claim wins: a PID already attributed elsewhere
                    // is not double-counted.
                    let taken: HashSet<u32> =
                        map.values().flatten().copied().collect();
                    let entry = map.entry(key).or_default();
                    for p in found {
                        if !taken.contains(&p) {
                            entry.insert(p);
                        }
                    }
                }
            }
        });
}

/// Drop an account's attribution once none of its windows remain tracked.
/// Idempotent — safe to call from both the close command and the
/// window-destroyed handler.
pub fn prune(app: &tauri::AppHandle, app_id: &str, account_id: &str) {
    let still_open = app
        .try_state::<crate::windows::WindowState>()
        .map(|ws| ws.has_account_window(app_id, account_id))
        .unwrap_or(false);
    if still_open {
        return;
    }
    if let Some(attr) = app.try_state::<ProcessAttribution>() {
        if let Ok(mut map) = attr.inner.lock() {
            map.remove(&account_key(app_id, account_id));
        }
    }
}

/// Pure core of the query: sum RSS for the attributed PIDs plus every
/// process whose ancestry reaches an attributed PID. `processes` is
/// (pid, parent_pid, rss_kb). Unit-tested with a fake tree.
fn attributed_rss(
    processes: &[(u32, Option<u32>, u64)],
    attributed: &HashSet<u32>,
) -> u64 {
    if attributed.is_empty() {
        return 0;
    }
    let parent_of: HashMap<u32, Option<u32>> =
        processes.iter().map(|(p, par, _)| (*p, *par)).collect();
    processes
        .iter()
        .filter(|(p, _, _)| {
            let mut cur = Some(*p);
            while let Some(c) = cur {
                if attributed.contains(&c) {
                    return true;
                }
                cur = parent_of.get(&c).copied().flatten();
            }
            false
        })
        .map(|(_, _, rss)| rss)
        .sum()
}

/// Per-account RSS for the tiles. One scan; accounts with no open window
/// (or no captured processes yet) are omitted.
#[tauri::command]
pub fn account_memory(app: tauri::AppHandle) -> Vec<AccountMemory> {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System};
    let sys = System::new_with_specifics(
        RefreshKind::nothing()
            .with_processes(ProcessRefreshKind::nothing().with_memory().without_tasks()),
    );
    let self_pid = std::process::id();
    let processes: Vec<(u32, Option<u32>, u64)> = sys
        .processes()
        .iter()
        .filter_map(|(pid, proc_)| {
            let p = pid.as_u32();
            if p == self_pid {
                return None;
            }
            Some((p, proc_.parent().map(|a| a.as_u32()), proc_.memory() / 1024))
        })
        .collect();
    let map: HashMap<String, HashSet<u32>> = app
        .try_state::<ProcessAttribution>()
        .and_then(|a| a.inner.lock().ok().map(|m| m.clone()))
        .unwrap_or_default();
    map.into_iter()
        .filter_map(|(key, pids)| {
            let rss = attributed_rss(&processes, &pids);
            let (app_id, account_id) = key.split_once(':')?;
            Some(AccountMemory {
                app_id: app_id.to_string(),
                account_id: account_id.to_string(),
                rss_kb: rss,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_format() {
        assert_eq!(account_key("gmail", "work"), "gmail:work");
    }

    /// Fake tree: 100=self(excluded from `processes`), 200=browser for
    /// acct A with renderer 201 and GPU 202 beneath it, 300=browser for
    /// acct B with renderer 301, 400=unrelated.
    fn tree() -> Vec<(u32, Option<u32>, u64)> {
        vec![
            (200, Some(100), 50_000),
            (201, Some(200), 120_000),
            (202, Some(200), 30_000),
            (300, Some(100), 40_000),
            (301, Some(300), 90_000),
            (400, Some(1), 10_000),
        ]
    }

    #[test]
    fn sums_browser_plus_descendants() {
        let a: HashSet<u32> = [200].into();
        assert_eq!(attributed_rss(&tree(), &a), 200_000);
        let b: HashSet<u32> = [300].into();
        assert_eq!(attributed_rss(&tree(), &b), 130_000);
    }

    #[test]
    fn unrelated_and_empty_are_zero() {
        let empty: HashSet<u32> = HashSet::new();
        assert_eq!(attributed_rss(&tree(), &empty), 0);
        let gone: HashSet<u32> = [999].into();
        assert_eq!(attributed_rss(&tree(), &gone), 0);
        // Attributing a renderer directly still counts its subtree only.
        let r: HashSet<u32> = [201].into();
        assert_eq!(attributed_rss(&tree(), &r), 120_000);
    }
}
