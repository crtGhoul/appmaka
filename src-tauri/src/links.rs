//! Link dispatcher: optionally open https links inside an AppMaka account.
//!
//! The OS hands the app https URLs when it is registered as a handler for
//! the `https` scheme (see `tauri.conf.json > plugins.deep-link.desktop`).
//! Everything here is gated behind an explicit opt-in toggle that defaults
//! to OFF: with the toggle off, incoming URLs are ignored entirely.
//!
//! When a URL arrives and the toggle is on:
//! - the host is matched against the saved rules (exact host or
//!   parent-domain match, longest rule wins);
//! - a match opens the rule's account window on a dedicated thread and
//!   steers it to the URL;
//! - no match emits `appmaka:link-no-rule` so the frontend can offer the
//!   "open in account" picker.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager};

use crate::store::AppStore;

/// One saved routing: links from `domain` open in this account.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkRule {
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub account_id: String,
}

/// Persisted link-dispatcher config (`linkrules.json` in the app data dir).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkConfig {
    #[serde(default)]
    pub opt_in: bool,
    #[serde(default)]
    pub rules: Vec<LinkRule>,
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("could not create app data dir: {e}"))?;
    Ok(dir.join("linkrules.json"))
}

fn load_config(app: &AppHandle) -> LinkConfig {
    let path = match config_path(app) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[appmaka] links: {e}");
            return LinkConfig::default();
        }
    };
    match fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        // Missing or unreadable file: start from defaults (opt-in OFF).
        Err(_) => LinkConfig::default(),
    }
}

fn save_config(app: &AppHandle, cfg: &LinkConfig) -> Result<(), String> {
    let path = config_path(app)?;
    let json =
        serde_json::to_string_pretty(cfg).map_err(|e| format!("could not serialize link config: {e}"))?;
    // Write-then-rename so a crash mid-write never leaves half a file.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| format!("could not write linkrules.json: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("could not write linkrules.json: {e}"))?;
    Ok(())
}

/// Normalize a domain for storage and matching: lowercase, no scheme, no
/// port, no path, no leading `www.`.
fn normalize_domain(raw: &str) -> String {
    let mut domain = raw.trim().to_lowercase();
    for scheme in ["https://", "http://"] {
        if let Some(rest) = domain.strip_prefix(scheme) {
            domain = rest.to_string();
            break;
        }
    }
    if let Some(i) = domain.find(['/', '?', '#']) {
        domain.truncate(i);
    }
    // Strip a port, but leave IPv6 literals alone.
    if !domain.contains(']') {
        if let Some(i) = domain.rfind(':') {
            domain.truncate(i);
        }
    }
    if let Some(rest) = domain.strip_prefix("www.") {
        domain = rest.to_string();
    }
    domain
}

/// Longest rule whose domain matches `host` exactly or as a parent domain
/// (`mail.example.com` matches rule `example.com`, never `ample.com`).
fn find_rule<'a>(rules: &'a [LinkRule], host: &str) -> Option<&'a LinkRule> {
    rules
        .iter()
        .filter(|r| !r.domain.is_empty() && (host == r.domain || host.ends_with(&format!(".{}", r.domain))))
        .max_by_key(|r| r.domain.len())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Read the current link-dispatcher config.
/// JS: `invoke("get_link_config")` -> `{ optIn, rules: [{ domain, appId, accountId }] }`
#[tauri::command]
pub fn get_link_config(app: AppHandle) -> LinkConfig {
    load_config(&app)
}

/// Flip the opt-in toggle.
/// JS: `invoke("set_link_opt_in", { enabled })` -> updated config
#[tauri::command]
pub fn set_link_opt_in(app: AppHandle, enabled: bool) -> Result<LinkConfig, String> {
    let mut cfg = load_config(&app);
    cfg.opt_in = enabled;
    save_config(&app, &cfg)?;
    Ok(cfg)
}

/// Add (or replace) the rule for a domain. The app and account must exist.
/// JS: `invoke("add_link_rule", { domain, appId, accountId })` -> updated rules
#[tauri::command]
pub fn add_link_rule(
    app: AppHandle,
    domain: String,
    app_id: String,
    account_id: String,
) -> Result<Vec<LinkRule>, String> {
    let domain = normalize_domain(&domain);
    if domain.is_empty() {
        return Err("That domain is empty.".to_string());
    }
    let store = app.state::<AppStore>();
    let web_app = store.get(&app_id)?;
    if !web_app.accounts.iter().any(|a| a.id == account_id) {
        return Err("Account not found.".to_string());
    }
    let mut cfg = load_config(&app);
    match cfg.rules.iter_mut().find(|r| r.domain == domain) {
        Some(existing) => {
            existing.app_id = app_id;
            existing.account_id = account_id;
        }
        None => cfg.rules.push(LinkRule {
            domain,
            app_id,
            account_id,
        }),
    }
    cfg.rules.sort_by(|a, b| a.domain.cmp(&b.domain));
    save_config(&app, &cfg)?;
    Ok(cfg.rules)
}

/// Remove the rule for a domain (normalized the same way as on add).
/// JS: `invoke("remove_link_rule", { domain })` -> updated rules
#[tauri::command]
pub fn remove_link_rule(app: AppHandle, domain: String) -> Result<Vec<LinkRule>, String> {
    let domain = normalize_domain(&domain);
    let mut cfg = load_config(&app);
    let before = cfg.rules.len();
    cfg.rules.retain(|r| r.domain != domain);
    if cfg.rules.len() == before {
        return Err("No rule saved for that domain.".to_string());
    }
    save_config(&app, &cfg)?;
    Ok(cfg.rules)
}

// ---------------------------------------------------------------------------
// Incoming-URL handling
// ---------------------------------------------------------------------------

/// Subscribe to deep-link URLs and handle a cold start (the OS launched this
/// instance with a URL argument). Call once from `.setup()` in main.rs.
pub fn register_link_handler(app: &AppHandle) {
    use tauri_plugin_deep_link::DeepLinkExt;

    let warm_app = app.clone();
    app.deep_link().on_open_url(move |event| {
        handle_incoming_urls(&warm_app, &event.urls());
    });

    // Cold start: the plugin parsed the launch argument during init, before
    // any listener existed, so pick it up here instead of waiting for an
    // event that already fired.
    if let Ok(Some(urls)) = app.deep_link().get_current() {
        handle_incoming_urls(app, &urls);
    }
}

fn handle_incoming_urls(app: &AppHandle, urls: &[url::Url]) {
    let cfg = load_config(app);
    if !cfg.opt_in {
        return;
    }
    for url in urls {
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        let host = url.host_str().unwrap_or("").to_lowercase();
        if host.is_empty() {
            continue;
        }
        let url_str = url.as_str().to_string();
        match find_rule(&cfg.rules, &host) {
            Some(rule) => dispatch_to_account(app, &url_str, &rule.app_id, &rule.account_id),
            None => {
                let _ = app.emit("appmaka:link-no-rule", serde_json::json!({ "url": url_str }));
            }
        }
    }
}

/// Open a URL in a chosen account (the link-picker flow): same dispatch as
/// a matched rule. JS: `invoke("open_link_in_account", { appId, accountId, url })`.
#[tauri::command]
pub fn open_link_in_account(app: AppHandle, app_id: String, account_id: String, url: String) {
    dispatch_to_account(&app, &url, &app_id, &account_id);
}

/// Open the account window and steer it to the URL on a dedicated thread.
///
/// Never on the calling thread: on Windows, building a window on the main
/// thread self-deadlocks and building on a sync IPC thread deadlocks
/// (wry#583) — the same rule as `spawn_oauth_modal` in windows.rs.
fn dispatch_to_account(app: &AppHandle, url: &str, app_id: &str, account_id: &str) {
    let app = app.clone();
    let url = url.to_string();
    let app_id = app_id.to_string();
    let account_id = account_id.to_string();
    let _ = std::thread::Builder::new()
        .name(format!("appmaka-link-{app_id}"))
        .spawn(move || {
            // State guards borrow the AppHandle and are not 'static, so they
            // are re-resolved here inside the thread instead of crossing the
            // spawn boundary.
            let store = app.state::<AppStore>();
            let adblock = app.state::<crate::adblock::AdblockState>();
            let winstate = app.state::<crate::windows::WindowState>();
            if let Err(e) =
                crate::windows::open_account(&app, &store, &adblock, &winstate, &app_id, &account_id, false)
            {
                eprintln!("[appmaka] link dispatch: could not open {app_id}/{account_id}: {e}");
                return;
            }
            navigate_account_window(&app, &app_id, &account_id, &url);
        });
}

/// Steer an account window to `url`, retrying for ~3s.
///
/// The window is brand new and its webview may not have a document yet; an
/// `eval` before first paint can be dropped, so keep trying. Assigning the
/// same href twice is a no-op in the browser, so repeats are harmless.
fn navigate_account_window(app: &AppHandle, app_id: &str, account_id: &str, url: &str) {
    let label = crate::windows::account_window_label(app_id, account_id);
    // JSON-encode the URL so quotes can never break out of the script.
    let js_url = serde_json::to_string(url).unwrap_or_else(|_| "\"about:blank\"".to_string());
    let script = format!("window.location.href = {js_url};");
    let mut window_seen = false;
    for _ in 0..10 {
        if let Some(window) = app.get_webview_window(&label) {
            window_seen = true;
            let _ = window.eval(script.as_str());
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    if !window_seen {
        eprintln!("[appmaka] link dispatch: window {label} never appeared; dropped {url}");
    }
}
