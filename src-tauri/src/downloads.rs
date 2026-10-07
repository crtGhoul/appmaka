//! In-app download manager for account windows.
//!
//! Clicking a download link inside an account window used to kick the user
//! out to their system browser. This module wires the webview's native
//! download flow (`WebviewWindowBuilder::on_download`) so the download
//! happens inside the account window's session — cookies and login stay
//! intact — and lands in the user's download folder.
//!
//! What it is: a download list with progress, open, show-in-folder, remove,
//! clear-finished, retry, in-place rename, persistent history, a
//! per-download save-location chooser, and a configurable destination
//! folder. What it is NOT: a file manager — there is no filesystem
//! browsing, no moving files between folders, no deleting files, and no
//! pause/cancel (Tauri 2's `DownloadEvent` exposes only Requested/Finished,
//! and true pause would need a side-channel downloader that loses the
//! page's cookies).
//!
//! Steady-state RAM is negligible: downloads live in a bounded in-memory
//! Vec (cap 100); a 500 ms poller thread only exists while a download is
//! active and exits on completion.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::webview::{DownloadEvent, Webview};
use tauri::{
    AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder,
};

/// Cap on download history; newest first. The same vec is persisted.
const MAX_DOWNLOADS: usize = 100;

/// How often the poller thread samples the destination file size.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Timeout for the single best-effort HEAD request that learns the total size.
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);

/// Window-label prefix for hidden retry windows; the suffix is the id of
/// the entry being retried.
const RETRY_LABEL_PREFIX: &str = "appmaka-retry-";

/// How long to wait for a retried URL to start an actual download before
/// admitting the page probably needs a login now.
const RETRY_TIMEOUT: Duration = Duration::from_secs(60);

/// Payload for `appmaka:download-progress` events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadEntry {
    pub id: String,
    pub filename: String,
    pub url: String,
    /// "active" | "complete" | "failed" | "interrupted"
    pub state: String,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
    pub path: String,
    /// Unix millis when the download started; 0 for entries predating it.
    #[serde(default)]
    pub started_at: u64,
    /// True when the user picked the location in the save dialog: the
    /// stored path is then the trust root (it may sit outside the default
    /// download folder).
    #[serde(default)]
    pub picked_by_user: bool,
    /// Plain-language note attached after the fact (e.g. a retry that
    /// could not be reproduced). Shown under the state line.
    #[serde(default)]
    pub note: Option<String>,
}

/// Persisted settings, stored as `<app-data>/downloads.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DownloadSettings {
    #[serde(default = "default_download_dir")]
    download_dir: PathBuf,
    /// "Ask where to save each file before downloading". Default off:
    /// every browser defaults it off, and interrupting every download
    /// would break the quiet-UI stance.
    #[serde(default)]
    ask_where_to_save: bool,
    /// Where the save dialog starts next time (the last folder the user
    /// picked). None until the chooser is used once.
    #[serde(default)]
    last_save_dir: Option<PathBuf>,
    /// "Show a notice when downloads finish". Default on.
    #[serde(default = "default_true")]
    show_completion_notice: bool,
}

fn default_true() -> bool {
    true
}

impl Default for DownloadSettings {
    fn default() -> Self {
        Self {
            download_dir: default_download_dir(),
            ask_where_to_save: false,
            last_save_dir: None,
            show_completion_notice: true,
        }
    }
}

/// What the Settings UI reads/writes (paths as strings, camelCase).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DownloadSettingsPayload {
    pub download_dir: String,
    pub ask_where_to_save: bool,
    pub show_completion_notice: bool,
}

struct DownloadStore {
    downloads: Vec<DownloadEntry>,
    settings: DownloadSettings,
    /// Retry bookkeeping, runtime-only (never persisted): download URL ->
    /// hidden retry window label, so `Finished` can close the window.
    pending_retries: HashMap<String, String>,
}

/// Managed state: `app.manage(DownloadState::load(app)?)`.
pub struct DownloadState {
    store: Mutex<DownloadStore>,
    settings_file: PathBuf,
    history_file: PathBuf,
}

fn default_download_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(profile) = std::env::var("USERPROFILE") {
            return PathBuf::from(profile).join("Downloads");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join("Downloads");
    }
    std::env::temp_dir()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Atomic write of an app-data JSON file: tmp file + rename, so a crash
/// can never leave a half-written file behind.
fn atomic_write_json(file: &Path, json: &str) -> Result<(), String> {
    let tmp = file.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| format!("write error: {e}"))?;
    fs::rename(&tmp, file).map_err(|e| format!("write error: {e}"))
}

/// Serialize the in-memory history. Call with the store lock held, right
/// after mutating, so the file never lags the list.
fn write_history_file(store: &DownloadStore, file: &Path) -> Result<(), String> {
    let json =
        serde_json::to_string_pretty(&store.downloads).map_err(|e| format!("serialize error: {e}"))?;
    atomic_write_json(file, &json)
}

/// Anything still marked active died with the last process — the engine
/// transfer is gone, so it can never finish. Mark it interrupted instead
/// of showing an eternal "Downloading…".
fn mark_interrupted(entries: &mut [DownloadEntry]) {
    for entry in entries.iter_mut() {
        if entry.state == "active" {
            entry.state = "interrupted".to_string();
        }
    }
}

impl DownloadState {
    /// Load settings from `<app-data>/downloads.json` and history from
    /// `<app-data>/download-history.json`, creating the download folder if
    /// it does not exist. Malformed or missing files fall back to defaults
    /// (serde defaults); a corrupt history file is renamed aside, never
    /// allowed to break startup.
    pub fn load(app: &AppHandle) -> Result<Self, String> {
        let dir = app
            .path()
            .app_data_dir()
            .map_err(|e| format!("could not resolve app data dir: {e}"))?;
        fs::create_dir_all(&dir).map_err(|e| format!("could not create app data dir: {e}"))?;
        let settings_file = dir.join("downloads.json");
        let history_file = dir.join("download-history.json");

        let settings = match fs::read_to_string(&settings_file) {
            Ok(contents) => serde_json::from_str::<DownloadSettings>(&contents)
                .unwrap_or_default(),
            Err(_) => DownloadSettings::default(),
        };

        let mut downloads: Vec<DownloadEntry> = match fs::read_to_string(&history_file) {
            Ok(contents) => match serde_json::from_str::<Vec<DownloadEntry>>(&contents) {
                Ok(entries) => entries,
                Err(_) => {
                    let _ = fs::rename(&history_file, dir.join("download-history.json.corrupt"));
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        mark_interrupted(&mut downloads);
        downloads.truncate(MAX_DOWNLOADS);
        // Persist the interrupted marking so a second crash right after
        // load cannot resurrect eternal "active" rows.
        let history_json = serde_json::to_string_pretty(&downloads)
            .map_err(|e| format!("serialize error: {e}"))?;
        let _ = atomic_write_json(&history_file, &history_json);

        fs::create_dir_all(&settings.download_dir).map_err(|e| {
            format!(
                "could not create download folder {}: {e}",
                settings.download_dir.display()
            )
        })?;

        Ok(Self {
            store: Mutex::new(DownloadStore {
                downloads,
                settings,
                pending_retries: HashMap::new(),
            }),
            settings_file,
            history_file,
        })
    }

    fn with_store<R>(
        &self,
        f: impl FnOnce(&mut DownloadStore) -> Result<R, String>,
    ) -> Result<R, String> {
        let mut store = self
            .store
            .lock()
            .map_err(|e| format!("download state lock poisoned: {e}"))?;
        f(&mut store)
    }
}

// ---------------------------------------------------------------------------
// Download handler
// ---------------------------------------------------------------------------

static DOWNLOAD_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_download_id() -> String {
    let millis = now_millis();
    let n = DOWNLOAD_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("dl-{millis}-{n}")
}

/// Strip anything that could escape the download folder: keep only the file
/// name component, drop separators, reject ".." and empty names.
fn sanitize_filename(raw: &str) -> String {
    let base = Path::new(raw)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    // file_name() already strips directory separators and rejects "..";
    // filter again as belt-and-suspenders against odd input.
    let cleaned: String = base
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | '\0'))
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned == "." {
        "download".to_string()
    } else {
        cleaned.to_string()
    }
}

/// Pick a destination under `dir` that does not clobber an existing file:
/// `name (1).ext`, `name (2).ext`, ...
fn unique_destination(dir: &Path, filename: &str) -> PathBuf {
    let first = dir.join(filename);
    if !first.exists() {
        return first;
    }
    let stem = Path::new(filename)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| filename.to_string());
    let ext = Path::new(filename)
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let mut n: u32 = 1;
    loop {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
        n = n.saturating_add(1);
    }
}

/// One best-effort HEAD request to learn the total size for the progress
/// display. Never downloads the body; failure just leaves `total_bytes`
/// unknown ("12 MB so far" instead of "12 MB of 48 MB").
fn head_content_length(url: &str) -> Option<u64> {
    let resp = ureq::head(url).timeout(HEAD_TIMEOUT).call().ok()?;
    resp.header("content-length")?.parse::<u64>().ok()
}

fn emit_progress(app: &AppHandle, entry: &DownloadEntry) {
    let _ = app.emit("appmaka:download-progress", entry);
}

/// Poll the destination file size while the download is active. Runs on its
/// own thread and exits as soon as the download leaves "active" — no
/// persistent threads.
fn spawn_progress_poller(app: AppHandle, id: String, url: String, dest: PathBuf) {
    let thread_name = format!("appmaka-dl-poller-{id}");
    let _ = std::thread::Builder::new().name(thread_name).spawn(move || {
        // One best-effort HEAD up front; after this we only read file sizes.
        if let Some(total) = head_content_length(&url) {
            if let Some(state) = app.try_state::<DownloadState>() {
                let _ = state.with_store(|store| {
                    if let Some(entry) = store.downloads.iter_mut().find(|e| e.id == id) {
                        entry.total_bytes = Some(total);
                    }
                    Ok(())
                });
            }
        }
        loop {
            std::thread::sleep(POLL_INTERVAL);
            let received = fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            let finished = match app.try_state::<DownloadState>() {
                Some(state) => {
                    let result = state.with_store(|store| {
                        if let Some(entry) = store.downloads.iter_mut().find(|e| e.id == id) {
                            entry.received_bytes = entry.received_bytes.max(received);
                            let active = entry.state == "active";
                            let entry = entry.clone();
                            Ok((entry, active))
                        } else {
                            Err("gone".to_string())
                        }
                    });
                    match result {
                        Ok((entry, active)) => {
                            if !active {
                                // The Finished handler already emitted the
                                // final state; no need to repeat it.
                                true
                            } else {
                                emit_progress(&app, &entry);
                                false
                            }
                        }
                        Err(_) => true,
                    }
                }
                None => true,
            };
            if finished {
                break;
            }
        }
    });
}

/// Ask the user where to save one download, with the native save dialog.
/// Called on the download callback thread, which is the MAIN thread on
/// both platforms (verified at runtime on Linux: `Requested` fires on
/// `ThreadId(1) name=main`; WebView2's DownloadStarting fires on the COM
/// UI thread on Windows).
///
/// This MUST NOT use tauri-plugin-dialog here: the plugin's blocking
/// wrappers post dialog creation to the main thread via `run_on_main_thread`
/// and then block waiting for it — instant deadlock when called FROM the
/// main thread (and the async variant cannot satisfy the synchronous
/// handler).
///
/// Platform strategy — both are "modal dialog on the calling thread",
/// the standard pattern, never a rendezvous with Tauri's event loop:
/// - Windows: rfd's sync api. `IFileSaveDialog::Show` runs its own modal
///   loop on the calling thread; COM init is balanced inside rfd.
/// - Linux: GTK is strictly main-thread-only, and rfd's second-GTK-thread
///   design hangs against tao's main loop (verified: `save_file()` never
///   returns and no dialog appears). So the FileChooserNative is built
///   directly on the main thread and a nested event loop is pumped until
///   the user responds — exactly what `gtk_dialog_run` does.
///
/// Returns None when the user cancels.
fn prompt_save_location(suggested_filename: &str, start_dir: Option<&Path>) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Save file")
            .set_file_name(suggested_filename);
        if let Some(dir) = start_dir {
            dialog = dialog.set_directory(dir);
        }
        dialog.save_file()
    }
    #[cfg(target_os = "linux")]
    {
        prompt_save_location_gtk(suggested_filename, start_dir)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (suggested_filename, start_dir);
        None
    }
}

/// Linux save dialog: a FileChooserNative driven directly on the GTK main
/// thread with a nested event pump. `gtk::main_iteration()` re-enters the
/// outer (tao) loop instead of blocking it, so the app stays alive while
/// the modal dialog is up. No locks are held across this call — the store
/// lock was released before the dialog.
#[cfg(target_os = "linux")]
fn prompt_save_location_gtk(suggested_filename: &str, start_dir: Option<&Path>) -> Option<PathBuf> {
    use gtk::prelude::*;
    use std::sync::{Arc, Mutex};

    let dialog = gtk::FileChooserNative::new(
        Some("Save file"),
        None::<&gtk::Window>,
        gtk::FileChooserAction::Save,
        Some("_Save"),
        Some("_Cancel"),
    );
    dialog.set_current_name(suggested_filename);
    if let Some(dir) = start_dir {
        let _ = dialog.set_current_folder(dir);
    }
    let result: Arc<Mutex<Option<Option<PathBuf>>>> = Arc::new(Mutex::new(None));
    let result_cb = Arc::clone(&result);
    dialog.connect_response(move |d, response| {
        let picked = if response == gtk::ResponseType::Accept {
            d.file().and_then(|f| f.path())
        } else {
            None
        };
        *result_cb.lock().unwrap() = Some(picked);
    });
    dialog.show();
    loop {
        if result.lock().unwrap().is_some() {
            break;
        }
        gtk::main_iteration();
    }
    dialog.hide();
    let picked = result.lock().unwrap().take().flatten();
    picked
}

/// Pure destination planning: no fs side effects, no dialogs, so the
/// choice logic is unit-testable. `dialog_pick` is None when the chooser
/// is off, Some(choice) when it is on. Returns None when the user
/// cancelled the save dialog — the caller must then deny the download.
fn plan_destination(
    download_dir: &Path,
    suggested_filename: &str,
    dialog_pick: Option<Option<PathBuf>>,
) -> Option<(PathBuf, String, bool)> {
    match dialog_pick {
        // The user cancelled the save dialog: no entry, no download.
        Some(None) => None,
        // The user picked a location: the OS dialog already confirmed any
        // overwrite, so the choice stands exactly as picked.
        Some(Some(picked)) => {
            let filename = picked
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| suggested_filename.to_string());
            Some((picked, filename, true))
        }
        // No chooser: classic path under the download folder.
        None => {
            let dest = unique_destination(download_dir, suggested_filename);
            let filename = dest
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| suggested_filename.to_string());
            Some((dest, filename, false))
        }
    }
}

/// Build the `on_download` closure for a window's `WebviewWindowBuilder`
/// chain. The webview itself performs the download (WebView2's
/// DownloadStarting on Windows, WebKitGTK's download-started on Linux), so
/// the page's session and cookies are preserved.
pub fn make_download_handler(
    app: AppHandle,
) -> impl Fn(Webview<tauri::Wry>, DownloadEvent<'_>) -> bool + Send + Sync + 'static {
    move |webview, event| match event {
        DownloadEvent::Requested { url, destination } => {
            let Some(state) = app.try_state::<DownloadState>() else {
                // DownloadState not managed yet; block rather than leak the
                // file to the webview's default location.
                return false;
            };
            let url_str = url.as_str().to_string();
            // A retry re-fires through a hidden window whose label names
            // the entry it supersedes.
            let retry_of: Option<String> = webview
                .label()
                .strip_prefix(RETRY_LABEL_PREFIX)
                .map(|s| s.to_string());

            let suggested = sanitize_filename(
                &destination
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "download".to_string()),
            );

            // Snapshot what the dialog needs, then release the lock: the
            // dialog is modal and must never run under our mutex.
            let snapshot = state.with_store(|store| {
                Ok::<_, String>((
                    store.settings.download_dir.clone(),
                    store.settings.ask_where_to_save,
                    store
                        .settings
                        .last_save_dir
                        .clone()
                        .or(Some(store.settings.download_dir.clone())),
                ))
            });
            let (download_dir, ask, start_dir) = match snapshot {
                Ok(v) => v,
                Err(_) => return false,
            };
            let history_file = state.history_file.clone();
            let settings_file = state.settings_file.clone();

            // Possibly modal (see prompt_save_location): no locks held here.
            let dialog_pick: Option<Option<PathBuf>> =
                ask.then(|| prompt_save_location(&suggested, start_dir.as_deref()));

            let Some((dest, filename, picked_by_user)) =
                plan_destination(&download_dir, &suggested, dialog_pick)
            else {
                return false;
            };

            let prepared = state.with_store(|store| {
                if picked_by_user {
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent)
                            .map_err(|e| format!("could not create folder: {e}"))?;
                        // The next dialog starts where the user saved.
                        store.settings.last_save_dir = Some(parent.to_path_buf());
                    }
                } else if let Err(e) = fs::create_dir_all(&download_dir) {
                    return Err(format!("could not create download folder: {e}"));
                }
                // A retry supersedes the failed entry it was launched from.
                if let Some(orig_id) = &retry_of {
                    store.downloads.retain(|e| e.id != *orig_id);
                    store
                        .pending_retries
                        .insert(url_str.clone(), webview.label().to_string());
                }
                let entry = DownloadEntry {
                    id: next_download_id(),
                    filename,
                    url: url_str.clone(),
                    state: "active".to_string(),
                    received_bytes: 0,
                    total_bytes: None,
                    path: dest.to_string_lossy().into_owned(),
                    started_at: now_millis(),
                    picked_by_user,
                    note: None,
                };
                // Newest first, bounded history.
                store.downloads.insert(0, entry.clone());
                store.downloads.truncate(MAX_DOWNLOADS);
                *destination = dest.clone();
                write_history_file(store, &history_file)?;
                if picked_by_user {
                    persist_settings(&settings_file, &store.settings)?;
                }
                Ok(entry)
            });
            match prepared {
                Ok(entry) => {
                    emit_progress(&app, &entry);
                    let dest = PathBuf::from(&entry.path);
                    spawn_progress_poller(app.clone(), entry.id.clone(), entry.url.clone(), dest);
                    true
                }
                Err(_) => false,
            }
        }
        DownloadEvent::Finished { url, path, success } => {
            let Some(state) = app.try_state::<DownloadState>() else {
                return true;
            };
            let url_str = url.as_str();
            let history_file = state.history_file.clone();
            let result = state.with_store(|store| {
                let entry = match store
                    .downloads
                    .iter_mut()
                    .find(|e| e.state == "active" && e.url == url_str)
                {
                    Some(e) => {
                        e.state = if success { "complete" } else { "failed" }.to_string();
                        // Prefer the final path reported by the platform.
                        if let Some(p) = path {
                            e.path = p.to_string_lossy().into_owned();
                        }
                        // Refresh the final size so a finished entry reports
                        // the full byte count even if the last poll missed it.
                        if success {
                            if let Ok(meta) = fs::metadata(Path::new(&e.path)) {
                                e.received_bytes = meta.len();
                            }
                        }
                        e.clone()
                    }
                    None => return Err("no active entry for this download".to_string()),
                };
                // A retry's hidden window has served its purpose.
                let retry_label = store.pending_retries.remove(url_str);
                write_history_file(store, &history_file)?;
                Ok((entry, retry_label))
            });
            if let Ok((entry, retry_label)) = result {
                if let Some(label) = retry_label {
                    if let Some(w) = app.get_webview_window(&label) {
                        let _ = w.close();
                    }
                }
                // Windows only: stamp the Mark of the Web. Browsers tag every
                // download with a Zone.Identifier stream carrying the source
                // URL so SmartScreen and Explorer's "this file came from the
                // internet" warnings keep working. Saving without it would be
                // a security regression versus every browser.
                #[cfg(windows)]
                if entry.state == "complete" {
                    write_zone_identifier(&entry.path, &entry.url);
                }
                // v0.11.0: failed downloads are recorded for Copy
                // diagnostics. The downloads page shows the failure itself,
                // so no dialog here.
                if entry.state == "failed" {
                    crate::errors::record(
                        &app,
                        "download",
                        &format!("The download failed: {}", entry.filename),
                        &format!("download failed: url={} path={}", entry.url, entry.path),
                        false,
                    );
                }
                emit_progress(&app, &entry);
            }
            true
        }
        // DownloadEvent is non-exhaustive; future variants default to the
        // webview's built-in behavior (allow).
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Mark of the Web (Windows only)
// ---------------------------------------------------------------------------

/// Write the `Zone.Identifier` alternate data stream next to a finished
/// download, exactly like Chrome/Edge/Firefox do: ZoneId=3 (internet zone)
/// with the source URL as HostUrl. SmartScreen and Explorer depend on it.
///
/// Best-effort by design: a failed ADS write must never fail or undo the
/// download itself. On Linux there is no equivalent marking, so this is a
/// no-op there.
#[cfg(windows)]
fn write_zone_identifier(path: &str, url: &str) {
    let ads_path = format!("{path}:Zone.Identifier");
    let content = format!("[ZoneTransfer]\r\nZoneId=3\r\nHostUrl={url}\r\n");
    let _ = std::fs::write(ads_path, content);
}

// ---------------------------------------------------------------------------
// Path validation
// ---------------------------------------------------------------------------

/// Resolve the stored download dir and return its canonical form for
/// starts_with checks.
fn canonical_download_dir(store: &DownloadStore) -> Result<PathBuf, String> {
    store
        .settings
        .download_dir
        .canonicalize()
        .map_err(|_| "The download folder is missing. Pick a new one below.".to_string())
}

/// Look up an entry and return its path. Entries downloaded to the default
/// folder must resolve inside it; entries the user placed via the save
/// dialog carry their own trust root (the exact picked path). Fails
/// closed: anything unresolvable is rejected. Never deletes anything —
/// validation is read-only.
fn validated_entry_path(store: &DownloadStore, id: &str) -> Result<PathBuf, String> {
    let entry = store
        .downloads
        .iter()
        .find(|e| e.id == id)
        .ok_or_else(|| "That download is not in the list.".to_string())?;
    let path = PathBuf::from(&entry.path)
        .canonicalize()
        .map_err(|_| "That file is no longer on disk.".to_string())?;
    if entry.picked_by_user {
        return Ok(path);
    }
    let dir = canonical_download_dir(store)?;
    if !path.starts_with(&dir) {
        return Err("That file is outside the download folder.".to_string());
    }
    Ok(path)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// JS: `invoke("list_downloads")`
#[tauri::command]
pub fn list_downloads(state: State<DownloadState>) -> Vec<DownloadEntry> {
    state
        .with_store(|store| Ok(store.downloads.clone()))
        .unwrap_or_default()
}

/// Open the file with the OS default app. Read-only validation: the path
/// must resolve inside the download folder (or be the exact path the user
/// picked in the save dialog).
///
/// JS: `invoke("open_download", { id })`
#[tauri::command]
pub fn open_download(app: AppHandle, state: State<DownloadState>, id: String) -> Result<(), String> {
    let path = state.with_store(|store| validated_entry_path(store, &id))?;
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_path(path.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|e| format!("Could not open the file: {e}"))
}

/// Reveal the file in the system file manager. Read-only validation.
///
/// JS: `invoke("show_in_folder", { id })`
#[tauri::command]
pub fn show_in_folder(app: AppHandle, state: State<DownloadState>, id: String) -> Result<(), String> {
    let path = state.with_store(|store| validated_entry_path(store, &id))?;
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|e| format!("Could not show the file: {e}"))
}

/// Remove the list entry only. The file on disk is never touched.
///
/// JS: `invoke("remove_download", { id })`
#[tauri::command]
pub fn remove_download(state: State<DownloadState>, id: String) -> Result<(), String> {
    let history_file = state.history_file.clone();
    state.with_store(|store| {
        let before = store.downloads.len();
        store.downloads.retain(|e| e.id != id);
        if store.downloads.len() == before {
            return Err("That download is not in the list.".to_string());
        }
        write_history_file(store, &history_file)?;
        Ok(())
    })
}

/// Drop every entry that is not actively downloading.
///
/// JS: `invoke("clear_finished")`
#[tauri::command]
pub fn clear_finished(state: State<DownloadState>) -> Result<(), String> {
    let history_file = state.history_file.clone();
    state.with_store(|store| {
        store.downloads.retain(|e| e.state == "active");
        write_history_file(store, &history_file)?;
        Ok(())
    })
}

/// Rename a downloaded file in place (click-to-rename, Arc's pattern).
/// Renames the file on disk too — same folder, never moved elsewhere.
/// The Mark-of-the-Web stream travels with a same-volume rename, so no
/// re-stamping is needed.
///
/// JS: `invoke("rename_download", { id, filename })`
#[tauri::command]
pub fn rename_download(
    state: State<DownloadState>,
    id: String,
    filename: String,
) -> Result<(), String> {
    if filename.trim().is_empty() {
        return Err("Type a file name first.".to_string());
    }
    let history_file = state.history_file.clone();
    state.with_store(|store| {
        let entry = store
            .downloads
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| "That download is not in the list.".to_string())?;
        let clean = sanitize_filename(filename.trim());
        let current = PathBuf::from(&entry.path);
        let parent = current
            .parent()
            .ok_or_else(|| "That file has no folder.".to_string())?;
        let dest = unique_destination(parent, &clean);
        if dest == current {
            return Ok(());
        }
        fs::rename(&current, &dest).map_err(|e| format!("Could not rename the file: {e}"))?;
        entry.filename = dest
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or(clean);
        entry.path = dest.to_string_lossy().into_owned();
        write_history_file(store, &history_file)?;
        Ok(())
    })
}

/// Re-fire a failed or interrupted download through the same in-app
/// handler. The engine owns transfers — there is no re-drive handle — so
/// a hidden window navigates to the URL and the normal `Requested` path
/// starts a fresh download, superseding the old entry. Async: building a
/// window inside a sync command deadlocks (wry#583; repo AGENTS.md).
///
/// Authenticated downloads may not survive the round-trip (no cookie
/// replay outside the webview): when nothing starts within RETRY_TIMEOUT
/// the entry says so plainly instead of faking it.
///
/// JS: `invoke("retry_download", { id })`
#[tauri::command]
pub async fn retry_download(
    app: AppHandle,
    state: State<'_, DownloadState>,
    id: String,
) -> Result<(), String> {
    let url = state.with_store(|store| {
        let entry = store
            .downloads
            .iter()
            .find(|e| e.id == id)
            .ok_or_else(|| "That download is not in the list.".to_string())?;
        if entry.state != "failed" && entry.state != "interrupted" {
            return Err("Only failed or interrupted downloads can be retried.".to_string());
        }
        Ok(entry.url.clone())
    })?;
    let parsed: url::Url = url
        .parse()
        .map_err(|_| "That download's URL is no longer valid.".to_string())?;
    let label = format!("{RETRY_LABEL_PREFIX}{id}");
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.close();
    }
    WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(parsed))
        .title("Retrying download")
        .visible(false)
        .skip_taskbar(true)
        .on_download(make_download_handler(app.clone()))
        .build()
        .map_err(|e| format!("Could not start the retry: {e}"))?;

    // Watchdog: the page may now render instead of downloading (login
    // expired, link rotted). If no download started, close the window and
    // leave an honest note on the entry.
    let watch_app = app.clone();
    let watch_id = id.clone();
    let watch_label = label.clone();
    let watch_url = url.clone();
    let history_file = state.history_file.clone();
    std::thread::Builder::new()
        .name(format!("appmaka-dl-retrywatch-{id}"))
        .spawn(move || {
            std::thread::sleep(RETRY_TIMEOUT);
            let Some(watch_state) = watch_app.try_state::<DownloadState>() else {
                return;
            };
            let started = watch_state.with_store(|store| {
                if store.downloads.iter().any(|e| e.id == watch_id) {
                    if let Some(entry) = store.downloads.iter_mut().find(|e| e.id == watch_id) {
                        entry.note =
                            Some("Couldn't retry — open the page and download it again.".to_string());
                    }
                    store.pending_retries.remove(&watch_url);
                    write_history_file(store, &history_file)?;
                    Ok(false)
                } else {
                    // The original entry is gone: a fresh download started
                    // and superseded it.
                    Ok(true)
                }
            });
            if started == Ok(false) {
                if let Some(w) = watch_app.get_webview_window(&watch_label) {
                    let _ = w.close();
                }
                if let Ok(Some(entry)) = watch_state.with_store(|store| {
                    Ok(store.downloads.iter().find(|e| e.id == watch_id).cloned())
                }) {
                    emit_progress(&watch_app, &entry);
                }
            }
        })
        .map_err(|e| format!("Could not start the retry: {e}"))?;
    Ok(())
}

/// JS: `invoke("get_download_dir")`
#[tauri::command]
pub fn get_download_dir(state: State<DownloadState>) -> Result<String, String> {
    state.with_store(|store| {
        Ok(store
            .settings
            .download_dir
            .to_string_lossy()
            .into_owned())
    })
}

/// Change the download folder. The path must exist and be a directory; the
/// new value is persisted to `<app-data>/downloads.json`.
///
/// JS: `invoke("set_download_dir", { path })`
#[tauri::command]
pub fn set_download_dir(state: State<DownloadState>, path: String) -> Result<(), String> {
    let path = PathBuf::from(path.trim());
    if path.as_os_str().is_empty() {
        return Err("Pick a folder first.".to_string());
    }
    if !path.exists() {
        return Err("That folder does not exist.".to_string());
    }
    if !path.is_dir() {
        return Err("That is not a folder.".to_string());
    }
    let settings = state.with_store(|store| {
        store.settings.download_dir = path.clone();
        Ok(store.settings.clone())
    })?;
    // Persist outside the mutex so a disk failure does not leave the
    // in-memory value out of sync: re-read under a fresh lock on failure.
    if let Err(e) = persist_settings(&state.settings_file, &settings) {
        let _ = state.with_store(|store| {
            if let Ok(old) = fs::read_to_string(&state.settings_file) {
                if let Ok(parsed) = serde_json::from_str::<DownloadSettings>(&old) {
                    store.settings = parsed;
                }
            }
            Ok(())
        });
        return Err(format!("Could not save the download folder: {e}"));
    }
    Ok(())
}

/// The Settings UI's view of the download configuration.
///
/// JS: `invoke("get_download_settings")`
#[tauri::command]
pub fn get_download_settings(state: State<DownloadState>) -> DownloadSettingsPayload {
    state
        .with_store(|store| {
            Ok(DownloadSettingsPayload {
                download_dir: store.settings.download_dir.to_string_lossy().into_owned(),
                ask_where_to_save: store.settings.ask_where_to_save,
                show_completion_notice: store.settings.show_completion_notice,
            })
        })
        .unwrap_or_default()
}

/// Toggle "Ask where to save each file before downloading".
///
/// JS: `invoke("set_ask_where_to_save", { enabled })`
#[tauri::command]
pub fn set_ask_where_to_save(state: State<DownloadState>, enabled: bool) -> Result<(), String> {
    let settings = state.with_store(|store| {
        store.settings.ask_where_to_save = enabled;
        Ok(store.settings.clone())
    })?;
    persist_settings(&state.settings_file, &settings)
}

/// Toggle the quiet "Download finished" notice.
///
/// JS: `invoke("set_show_completion_notice", { enabled })`
#[tauri::command]
pub fn set_show_completion_notice(
    state: State<DownloadState>,
    enabled: bool,
) -> Result<(), String> {
    let settings = state.with_store(|store| {
        store.settings.show_completion_notice = enabled;
        Ok(store.settings.clone())
    })?;
    persist_settings(&state.settings_file, &settings)
}

/// Let the user pick the default download folder with the native folder
/// picker. Async with spawn_blocking: the blocking picker must never run
/// on the async runtime. Returns Ok(None) on cancel.
///
/// JS: `invoke("pick_download_dir")`
#[tauri::command]
pub async fn pick_download_dir(app: AppHandle) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use tauri_plugin_dialog::DialogExt;
        let picked = app.dialog().file().blocking_pick_folder();
        Ok::<_, String>(picked.map(|fp| fp.to_string()))
    })
    .await
    .map_err(|e| format!("folder picker failed: {e}"))?
}

fn persist_settings(file: &Path, settings: &DownloadSettings) -> Result<(), String> {
    let json =
        serde_json::to_string_pretty(settings).map_err(|e| format!("serialize error: {e}"))?;
    atomic_write_json(file, &json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_drops_separators_and_dotdot() {
        assert_eq!(sanitize_filename("/etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("../../evil.exe"), "evil.exe");
        assert_eq!(sanitize_filename(".."), "download");
        assert_eq!(sanitize_filename(""), "download");
        assert_eq!(sanitize_filename("report (1).pdf"), "report (1).pdf");
        assert_eq!(sanitize_filename("."), "download");
    }

    #[test]
    fn dedup_avoids_clobbering() {
        let dir = std::env::temp_dir().join("appmaka-dl-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_destination(&dir, "a.txt"), dir.join("a (1).txt"));
        fs::write(dir.join("a (1).txt"), b"x").unwrap();
        assert_eq!(unique_destination(&dir, "a.txt"), dir.join("a (2).txt"));
        assert_eq!(unique_destination(&dir, "fresh.bin"), dir.join("fresh.bin"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_defaults_to_a_real_folder() {
        let s = DownloadSettings::default();
        assert!(s.download_dir.is_absolute());
    }

    #[test]
    fn settings_serde_defaults() {
        // An old downloads.json with only download_dir still loads, with
        // the new toggles at their defaults (chooser off, notice on).
        let s: DownloadSettings = serde_json::from_str(r#"{"download_dir": "/tmp"}"#).unwrap();
        assert!(!s.ask_where_to_save);
        assert!(s.show_completion_notice);
        assert!(s.last_save_dir.is_none());
    }

    #[test]
    fn entry_serde_defaults() {
        // Entries written before started_at/picked_by_user/note existed
        // still parse.
        let e: DownloadEntry = serde_json::from_str(
            r#"{"id":"dl-1","filename":"a.pdf","url":"https://x/y","state":"complete","receivedBytes":10,"totalBytes":10,"path":"/tmp/a.pdf"}"#,
        )
        .unwrap();
        assert_eq!(e.started_at, 0);
        assert!(!e.picked_by_user);
        assert!(e.note.is_none());
    }

    #[test]
    fn plan_destination_cancel_means_no_download() {
        let dir = Path::new("/tmp");
        assert!(plan_destination(dir, "a.pdf", Some(None)).is_none());
    }

    #[test]
    fn plan_destination_honors_the_picked_path() {
        let dir = Path::new("/tmp");
        let picked = PathBuf::from("/home/user/docs/report.pdf");
        let (dest, filename, picked_by_user) =
            plan_destination(dir, "report.pdf", Some(Some(picked.clone()))).unwrap();
        assert_eq!(dest, picked);
        assert_eq!(filename, "report.pdf");
        assert!(picked_by_user);
    }

    #[test]
    fn plan_destination_default_stays_under_download_dir() {
        let dir = std::env::temp_dir().join("appmaka-dl-plan");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (dest, filename, picked_by_user) =
            plan_destination(&dir, "a.pdf", None).unwrap();
        assert_eq!(dest, dir.join("a.pdf"));
        assert_eq!(filename, "a.pdf");
        assert!(!picked_by_user);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn interrupted_marking_never_leaves_eternal_active() {
        let mut entries = vec![
            DownloadEntry {
                id: "a".into(),
                filename: "a".into(),
                url: "u".into(),
                state: "active".into(),
                received_bytes: 0,
                total_bytes: None,
                path: "p".into(),
                started_at: 0,
                picked_by_user: false,
                note: None,
            },
            DownloadEntry {
                id: "b".into(),
                filename: "b".into(),
                url: "u".into(),
                state: "complete".into(),
                received_bytes: 0,
                total_bytes: None,
                path: "p".into(),
                started_at: 0,
                picked_by_user: false,
                note: None,
            },
        ];
        mark_interrupted(&mut entries);
        assert_eq!(entries[0].state, "interrupted");
        assert_eq!(entries[1].state, "complete");
    }

    #[test]
    fn history_round_trip_is_atomic() {
        let dir = std::env::temp_dir().join("appmaka-dl-hist");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("download-history.json");
        let store = DownloadStore {
            downloads: vec![DownloadEntry {
                id: "dl-9".into(),
                filename: "x.bin".into(),
                url: "https://x/y".into(),
                state: "interrupted".into(),
                received_bytes: 5,
                total_bytes: Some(10),
                path: "/tmp/x.bin".into(),
                started_at: 123,
                picked_by_user: true,
                note: None,
            }],
            settings: DownloadSettings::default(),
            pending_retries: HashMap::new(),
        };
        write_history_file(&store, &file).unwrap();
        // No tmp file left behind: the rename completed.
        assert!(!file.with_extension("json.tmp").exists());
        let back: Vec<DownloadEntry> =
            serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, "dl-9");
        assert_eq!(back[0].state, "interrupted");
        assert!(back[0].picked_by_user);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn picked_paths_validate_outside_the_download_dir() {
        let outside = std::env::temp_dir().join("appmaka-dl-outside");
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&outside).unwrap();
        let real = outside.join("chosen.pdf");
        fs::write(&real, b"x").unwrap();
        let mk = |picked: bool| DownloadStore {
            downloads: vec![DownloadEntry {
                id: "e".into(),
                filename: "chosen.pdf".into(),
                url: "https://x/y".into(),
                state: "complete".into(),
                received_bytes: 1,
                total_bytes: Some(1),
                path: real.to_string_lossy().into_owned(),
                started_at: 0,
                picked_by_user: picked,
                note: None,
            }],
            settings: DownloadSettings {
                download_dir: std::env::temp_dir().join("appmaka-dl-dir"),
                ..DownloadSettings::default()
            },
            pending_retries: HashMap::new(),
        };
        // The exact path the user picked in the save dialog is trusted.
        assert!(validated_entry_path(&mk(true), "e").is_ok());
        // The same path without the dialog flag is rejected: it sits
        // outside the download folder.
        assert!(validated_entry_path(&mk(false), "e").is_err());
        let _ = fs::remove_dir_all(&outside);
    }
}
