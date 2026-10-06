//! Preview sign-in flow: open a site in a throwaway session, sign in on the
//! real page, then adopt the session as a new app's first account.
//!
//! The preview is TWO plain windows, both built with the stable
//! `WebviewWindowBuilder` API (never the `unstable` multi-webview API —
//! robustness beats elegance):
//! - the site window: the real page in a temporary session directory,
//!   `sessions/.preview-<id>/`;
//! - the control window: a small always-on-top window with our own UI — the
//!   URL, an optional account label, Add/Discard buttons (from
//!   `public/preview-header.html`). Buttons are never injected into the
//!   site's DOM, and the control window is the only preview webview allowed
//!   to invoke commands (see `capabilities/preview.json`).
//!
//! The temp session is either moved into place as the new account's session
//! dir ("Add to the Forge") or deleted ("Discard", either window closed by hand,
//! or left behind by a crash and swept at startup).
//!
//! Windows file-lock ordering: WebView2 locks the user-data dir while the
//! webview lives, so "Add to the Forge" closes the preview windows *before* moving
//! the directory, with a short retry loop in case teardown lags behind.
//!
//! Window creation never happens on a WebView2 IPC thread: on Windows,
//! `WebviewWindowBuilder::build()` deadlocks in a synchronous Tauri command
//! (wry#583), so `preview_start` is an async command (see main.rs).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
#[cfg(windows)]
use std::sync::{atomic::AtomicBool, Arc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::adblock::AdblockState;
use crate::store::{AddAppOutcome, AppStore};
use crate::windows::{self, PopupContext};

/// How long "Add to the Forge" waits for WebView2 to release the temp dir.
const MOVE_RETRIES: u32 = 30;
const MOVE_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Event the library window listens for so it can pick up the new app (or
/// reveal the existing one when the previewed site was already added).
const PREVIEW_ADDED_EVENT: &str = "appmaka:preview-added";

/// Event the library window listens for so it can clear its "preview opened"
/// notice when a preview is discarded or closed by hand.
const PREVIEW_CLOSED_EVENT: &str = "appmaka:preview-closed";

/// Control window size in physical pixels (used for placement math).
/// Tall enough for the sign-in banner row above the controls row.
const CONTROL_W: i32 = 940;
const CONTROL_H: i32 = 200;

#[derive(Debug, Clone)]
struct PreviewSession {
    url: String,
    session_dir: PathBuf,
    /// Latest non-empty document.title seen in the site window.
    title: Option<String>,
}

/// Live preview sessions, managed as Tauri state.
#[derive(Default)]
pub struct PreviewState {
    inner: Mutex<HashMap<String, PreviewSession>>,
}

/// Returned by `preview_start` so the frontend can report status.
#[derive(Serialize)]
pub struct PreviewStart {
    pub id: String,
    pub url: String,
}

/// Returned by `preview_add`: the app, whether it was freshly created, and
/// the account that adopted the preview's signed-in session — the first
/// account for a new app, or a brand-new account when the site was already
/// in the library (no duplicate app is ever created). Serialized camelCase
/// for the JS side (`addedAccount`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewAddOutcome {
    pub app: crate::store::WebApp,
    pub created: bool,
    pub added_account: Option<crate::store::Account>,
}

static PREVIEW_COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_preview_id() -> String {
    let n = PREVIEW_COUNTER.fetch_add(1, Ordering::Relaxed);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{ms}-{n}")
}

fn site_window_label(id: &str) -> String {
    format!("preview-{id}-site")
}

fn control_window_label(id: &str) -> String {
    format!("preview-{id}-header")
}

/// Extract the preview id from either preview window label.
fn preview_id_from_label(label: &str) -> Option<String> {
    let rest = label.strip_prefix("preview-")?;
    rest.strip_suffix("-site")
        .or_else(|| rest.strip_suffix("-header"))
        .map(str::to_string)
}

fn take_session(app: &AppHandle, preview_id: &str) -> Option<PreviewSession> {
    let state = app.try_state::<PreviewState>()?;
    let mut sessions = state.inner.lock().ok()?;
    sessions.remove(preview_id)
}

fn close_preview_windows(app: &AppHandle, preview_id: &str) {
    for label in [site_window_label(preview_id), control_window_label(preview_id)] {
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.close();
        }
    }
}

/// Open the preview for `url`: the site window (throwaway session) plus the
/// small always-on-top control window with Add/Discard. The caller signs in
/// on the real site; nothing is persisted until "Add to the Forge".
pub fn start_preview(
    app: &AppHandle,
    adblock: &AdblockState,
    url: &str,
) -> Result<PreviewStart, String> {
    let url = url.trim().to_string();
    if !crate::store::is_valid_url(&url) {
        return Err("URL must start with http:// or https://.".to_string());
    }
    // The store validates URLs on write, so this only fails on a hand-built
    // string that passed the looser check above — still no unwrap.
    let page_url: url::Url = url
        .parse()
        .map_err(|_| format!("App URL is not valid: {url}"))?;

    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?;
    let id = new_preview_id();
    let session_dir = data_dir.join("sessions").join(format!(".preview-{id}"));
    std::fs::create_dir_all(&session_dir)
        .map_err(|e| format!("could not create preview session dir: {e}"))?;

    // Clean up the temp dir if anything below fails.
    let build_result: Result<(), String> = (|| {
        // Site window: the real page in the throwaway session. Popups are
        // allowed as contained modals (never real windows) bound to this same
        // temp session — sign-in flows often use them, and the session is
        // discarded unless the user clicks "Add to the Forge".
        let app_for_title = app.clone();
        let title_id = id.clone();
        let mut site_builder = WebviewWindowBuilder::new(
            app,
            site_window_label(&id),
            WebviewUrl::External(page_url),
        )
        .data_directory(session_dir.clone())
        .title(format!("AppMaka Preview — {url}"))
        .inner_size(1200.0, 800.0)
        .center()
        .on_new_window(windows::make_popup_handler(PopupContext {
            app: app.clone(),
            app_id: format!("preview-{id}"),
            account_id: "preview".to_string(),
            app_name: "Preview".to_string(),
            app_url: url.clone(),
            session_dir: session_dir.clone(),
            popup_policy: "allow".to_string(),
            popup_allowlist: Vec::new(),
        }))
        .on_document_title_changed(move |_window, title: String| {
            let title = title.trim().to_string();
            if title.is_empty() {
                return;
            }
            if let Some(state) = app_for_title.try_state::<PreviewState>() {
                if let Ok(mut sessions) = state.inner.lock() {
                    if let Some(s) = sessions.get_mut(&title_id) {
                        s.title = Some(title.chars().take(160).collect());
                    }
                }
            }
        });
        let css = adblock.cosmetic_css_for(&url);
        if !css.is_empty() {
            site_builder =
                site_builder.initialization_script(windows::cosmetic_init_script(&css));
        }
        let site = site_builder
            .build()
            .map_err(|e| format!("could not open preview window: {e}"))?;

        // Previews always get ad blocking; there are no per-app settings yet.
        #[cfg(windows)]
        crate::adblock::attach_network_blocking(
            &site,
            adblock,
            Arc::new(AtomicBool::new(true)),
        );

        // Control window: our own UI, parked just above the site window and
        // kept on top so Add/Discard are always one click away. If placement
        // fails for any reason, a centered window is still perfectly usable.
        let (cx, cy) = match (site.outer_position(), site.inner_size()) {
            (Ok(pos), Ok(size)) => (
                pos.x + (size.width as i32 - CONTROL_W) / 2,
                (pos.y - CONTROL_H - 12).max(0),
            ),
            _ => (120, 80),
        };
        let json_id = serde_json::to_string(&id).unwrap_or_else(|_| "\"\"".to_string());
        let json_url = serde_json::to_string(&url).unwrap_or_else(|_| "\"\"".to_string());
        let control = WebviewWindowBuilder::new(
            app,
            control_window_label(&id),
            WebviewUrl::App("preview-header.html".into()),
        )
        .title("AppMaka Preview")
        .inner_size(CONTROL_W as f64, CONTROL_H as f64)
        .always_on_top(true)
        .initialization_script(format!(
            "window.__APPMAKA_PREVIEW_ID__={json_id};\
             window.__APPMAKA_PREVIEW_URL__={json_url};"
        ))
        .build()
        .map_err(|e| format!("could not open preview controls: {e}"))?;
        let _ = control.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
            x: cx,
            y: cy,
        }));

        Ok(())
    })();
    if let Err(e) = build_result {
        close_preview_windows(app, &id);
        let _ = std::fs::remove_dir_all(&session_dir);
        return Err(e);
    }

    {
        let state = app
            .try_state::<PreviewState>()
            .ok_or_else(|| "preview state not initialized.".to_string())?;
        let mut sessions = state
            .inner
            .lock()
            .map_err(|e| format!("preview state lock poisoned: {e}"))?;
        sessions.insert(
            id.clone(),
            PreviewSession {
                url: url.clone(),
                session_dir,
                title: None,
            },
        );
    }

    Ok(PreviewStart { id, url })
}

/// Abandon a preview: close its windows, drop the session, delete the temp
/// dir. The temp dir deletion is best-effort (a lagging WebView2 teardown
/// must not fail the command); leftovers are swept at startup.
pub fn discard_preview(app: &AppHandle, preview_id: &str) -> Result<(), String> {
    let session = take_session(app, preview_id);
    // Close first so WebView2 releases its file locks, then delete.
    close_preview_windows(app, preview_id);
    if let Some(s) = session {
        let _ = std::fs::remove_dir_all(&s.session_dir);
    }
    let _ = app.emit(PREVIEW_CLOSED_EVENT, preview_id);
    Ok(())
}

/// Reload the preview's site window (the control strip's refresh button).
/// Only the site webview is reloaded; the temp session is untouched.
pub fn reload_preview(app: &AppHandle, preview_id: &str) -> Result<(), String> {
    let window = app
        .get_webview_window(&site_window_label(preview_id))
        .ok_or_else(|| "Preview not found.".to_string())?;
    window
        .eval("window.location.reload()")
        .map_err(|e| format!("could not reload preview: {e}"))
}

/// Turn a preview into a real app: the temp session dir becomes the new
/// app's first account session dir, preserving the sign-in the user just
/// completed. The preview windows are closed *before* the move so WebView2
/// releases its locks on the directory. If the site is already in the
/// library, no duplicate is created — the temp session is deleted and the
/// existing app is returned for the UI to reveal.
pub fn add_preview_as_app(
    app: &AppHandle,
    store: &AppStore,
    preview_id: &str,
    label: Option<String>,
) -> Result<PreviewAddOutcome, String> {
    let session = take_session(app, preview_id).ok_or_else(|| "Preview not found.".to_string())?;
    let label = label.unwrap_or_default().trim().to_string();

    // Name: the live document.title wins (it sees JS-rendered titles), then
    // the plain HTTP title fetch, then a prettified domain.
    let name = match session.title.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => match crate::page_title::fetch_page_title(&session.url) {
            Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => prettified_domain(&session.url),
        },
    };

    close_preview_windows(app, preview_id);
    let AddAppOutcome {
        app: created,
        created: is_new,
        added_account,
    } = store.add_app_with_session(name, session.url, label, &session.session_dir)?;

    // The temp session is always adopted — never deleted here. A new app
    // keeps it as the first account; an already-listed site keeps it as a
    // new account on the existing app.

    // The library window picks the new app up (and opens the adopted
    // account), or updates the existing entry with its new account.
    let outcome = PreviewAddOutcome {
        app: created,
        created: is_new,
        added_account,
    };
    let _ = app.emit(PREVIEW_ADDED_EVENT, &outcome);
    let _ = app.emit(PREVIEW_CLOSED_EVENT, preview_id);
    Ok(outcome)
}

/// Best-effort cleanup when the user closes a preview window by hand (the X
/// button on either window): close the sibling window, drop the session,
/// delete the temp dir. `preview_add`/`preview_discard` already removed their
/// state entries, so those paths no-op here.
pub fn window_closed(app: &AppHandle, label: &str) {
    let Some(id) = preview_id_from_label(label) else {
        return;
    };
    close_preview_windows(app, &id);
    if let Some(session) = take_session(app, &id) {
        let _ = std::fs::remove_dir_all(&session.session_dir);
    }
    let _ = app.emit(PREVIEW_CLOSED_EVENT, id);
}

/// Delete leftover `.preview-*` temp dirs (crash safety). Best-effort.
pub fn cleanup_stale_previews(app: &AppHandle) {
    let Ok(data_dir) = app.path().app_data_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(data_dir.join("sessions")) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(".preview-") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Move a preview temp dir into its final home. Retries briefly because the
/// WebView2 teardown can lag behind the window close on Windows. Falls back
/// to a fresh empty dir (fail-open: the app is still created, the user just
/// signs in again) rather than failing the whole add.
pub(crate) fn move_session_dir(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create session dir: {e}"))?;
    }
    if !src.exists() {
        // Nothing to move (already cleaned up); start fresh.
        std::fs::create_dir_all(dst).map_err(|e| format!("could not create session dir: {e}"))?;
        return Ok(());
    }
    let mut last_err = String::new();
    for _ in 0..MOVE_RETRIES {
        match std::fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = e.to_string();
                std::thread::sleep(MOVE_RETRY_DELAY);
            }
        }
    }
    eprintln!("[appmaka] preview session move failed after retries: {last_err}; starting with a fresh session");
    std::fs::create_dir_all(dst).map_err(|e| format!("could not create session dir: {e}"))?;
    Ok(())
}

pub(crate) fn prettified_domain(raw_url: &str) -> String {
    let host = url::Url::parse(raw_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let mut chars = host.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => raw_url.to_string(),
    }
}
