//! Clipboard history v2 (text + images).
//!
//! A poll-based clipboard watcher records text and image copies into a
//! local, newest-first history stored in `<app-data>/clipboard.json`.
//! Image bytes live as PNG files under `<app-data>/clipboard_images/`;
//! the JSON keeps only metadata. Nothing is ever transmitted anywhere —
//! there is no network code in this module.
//!
//! Design notes:
//! - The watcher is a single tokio task ticking every 600ms. Each tick is
//!   one text read + compare and one image probe (cheap when the clipboard
//!   holds no image; when it does, a sampled fingerprint avoids hashing
//!   tens of megabytes every tick). No new processes, no background
//!   services.
//! - v2 records TEXT and IMAGES (screenshots, copied pictures). Empty/
//!   whitespace-only texts are ignored, texts over 1 MiB are skipped, and
//!   images over 40 MiB raw are skipped (a 4K screenshot is ~33 MiB RGBA).
//! - Selecting an entry copies it back to the OS clipboard and closes the
//!   popup. v2 does NOT synthesize Ctrl+V into other apps — the user
//!   pastes normally after picking.
//! - The popup window is built on a dedicated thread, never inside a
//!   synchronous command and never on the main thread (same Windows
//!   WebView2 deadlock rule as every other window in this app).

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::hotkeys::{record_binding_failure, set_binding, BindingStatus, HotkeyKind};
#[cfg(windows)]
use crate::hotkeys::remove_binding;

/// Registry binding id for the clipboard popup hotkey.
pub const BINDING_ID: &str = "clipboard:popup";
/// Window label for the clipboard popup.
pub const WINDOW_LABEL: &str = "clipboard";
/// Default global hotkey for the popup. The user asked for the Windows key.
/// Win+Shift+V was verified reserved (the shell cycles notifications with
/// it, and PowerToys' Advanced Paste also claims it), so the default is
/// Win+Alt+V — free of any documented Windows or PowerToys reservation.
pub const DEFAULT_HOTKEY: &str = "Super+Alt+V";
/// v0.9.0's default. Users still on it are migrated to the new default on
/// load (they asked for the Windows key; stranding them on Ctrl+Shift+V
/// would ignore that).
const OLD_DEFAULT_HOTKEY: &str = "Ctrl+Shift+V";
/// Default history cap (text entries). The text cap stays user-configurable.
pub const DEFAULT_CAP: usize = 100;
/// Image history cap (fixed; images are heavy, so this isn't a setting).
pub const IMAGE_CAP: usize = 25;
/// Minimum/maximum configurable text cap.
pub const MIN_CAP: usize = 10;
pub const MAX_CAP: usize = 1000;
/// Texts larger than this are never recorded.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
/// Images larger than this (raw RGBA) are never recorded. A 4K screenshot
/// is ~33 MiB, so 40 MiB admits real screenshots without letting a
/// gigapixel copy blow up the disk.
pub const MAX_IMAGE_BYTES: usize = 40 * 1024 * 1024;
/// Directory (under the app data dir) holding recorded image PNGs.
const IMAGES_DIR: &str = "clipboard_images";
/// How much of a text entry the list shows; the full text stays on disk
/// and is what gets copied back.
pub const PREVIEW_CHARS: usize = 500;
/// Watcher poll interval.
const POLL_INTERVAL: Duration = Duration::from_millis(600);

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

static ENTRY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// What kind of copy an entry holds. Defaults to Text so v0.9.0's
/// text-only history files load unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    #[default]
    Text,
    Image,
}

/// One recorded copy. Text entries carry `text`; image entries carry a PNG
/// file name (under `clipboard_images/`) plus dimensions. New fields all
/// default so old history files deserialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardEntry {
    pub id: String,
    #[serde(default)]
    pub kind: EntryKind,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub image_file: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    /// Raw RGBA byte size at record time (for the meta line).
    #[serde(default)]
    pub bytes: Option<u64>,
    /// Sampled fingerprint of the image, used to suppress re-recording.
    #[serde(default)]
    pub img_hash: Option<u64>,
    /// v0.9.12: pinned by the user in the popup. Defaults off so old
    /// history files keep every entry unpinned.
    #[serde(default)]
    pub pinned: bool,
    pub created_at_ms: u64,
}

/// What `list_clipboard` returns: newest first. Texts are truncated to a
/// preview; images expose an absolute file path (the UI turns it into a
/// webview-loadable URL with convertFileSrc) plus dimensions. Copying uses
/// the id, so full contents never have to travel to the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardListEntry {
    pub id: String,
    pub kind: String,
    pub preview: String,
    pub chars: usize,
    pub truncated: bool,
    /// v0.9.12: whether the user pinned this entry.
    pub pinned: bool,
    pub created_at_ms: u64,
    pub image_path: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Settings payload for the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardSettings {
    pub cap: usize,
    pub hotkey: String,
    /// v0.9.2: true when the popup is summoned by tapping the bare Windows
    /// key instead of a combo hotkey.
    pub win_tap: bool,
    /// The Win-key tap needs a low-level keyboard hook: Windows only.
    pub win_tap_supported: bool,
    /// v0.9.4: popup tab ("all" | "text" | "image"), persisted across
    /// summons and restarts.
    pub popup_tab: String,
    /// v0.9.12: "Pinned only" filter switch in the popup header, persisted
    /// like the tab.
    pub popup_pinned_only: bool,
}

/// On-disk shape of clipboard.json. Unknown fields are ignored on load so
/// future versions can extend it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClipboardFile {
    #[serde(default)]
    entries: Vec<ClipboardEntry>,
    #[serde(default = "default_cap")]
    cap: usize,
    #[serde(default = "default_hotkey")]
    hotkey: String,
    /// v0.9.2: summon via bare-Windows-key tap (Windows only) instead of
    /// the combo hotkey. Defaults off so old files keep combo behavior.
    #[serde(default)]
    win_tap: bool,
    /// v0.9.4: popup tab ("all" | "text" | "image"). Defaults to the mixed
    /// list so old files behave exactly as before.
    #[serde(default = "default_popup_tab")]
    popup_tab: String,
    /// v0.9.12: "Pinned only" filter. Defaults off so old files show the
    /// full history, exactly as before.
    #[serde(default)]
    popup_pinned_only: bool,
}

fn default_cap() -> usize {
    DEFAULT_CAP
}

fn default_hotkey() -> String {
    DEFAULT_HOTKEY.to_string()
}

/// Popup tab when the file predates it (or carries a value from the
/// future): the mixed list.
fn default_popup_tab() -> String {
    "all".to_string()
}

/// Tabs the popup understands; anything else (a hand-edited file, a
/// future value) falls back to the mixed list, same as a fresh install.
fn normalize_popup_tab(tab: &str) -> String {
    match tab {
        "text" | "image" => tab.to_string(),
        _ => "all".to_string(),
    }
}

impl Default for ClipboardFile {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            cap: DEFAULT_CAP,
            hotkey: DEFAULT_HOTKEY.to_string(),
            win_tap: false,
            popup_tab: default_popup_tab(),
            popup_pinned_only: false,
        }
    }
}

/// In-memory state. `last_seen` is the last clipboard text observed (or
/// written by us), `last_image_fp` the fingerprint of the last image —
/// neither is persisted; they only suppress re-recording.
struct ClipboardData {
    entries: VecDeque<ClipboardEntry>,
    cap: usize,
    hotkey: String,
    win_tap: bool,
    /// v0.9.4: popup tab, persisted in clipboard.json.
    popup_tab: String,
    /// v0.9.12: "Pinned only" filter, persisted in clipboard.json.
    popup_pinned_only: bool,
    last_seen: Option<String>,
    last_image_fp: Option<u64>,
}

pub struct ClipboardState {
    inner: Mutex<ClipboardData>,
}

fn clipboard_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("clipboard.json"))
}

/// One-time migration: users still on v0.9.0's default hotkey are carried
/// to the new Windows-key default (they asked for the Windows key).
/// Anything the user chose deliberately is left alone.
fn migrate_hotkey(saved: &str) -> String {
    if saved == OLD_DEFAULT_HOTKEY {
        DEFAULT_HOTKEY.to_string()
    } else {
        saved.to_string()
    }
}

/// Load persisted history, or start empty. A missing/corrupt file is just
/// "no history yet" — the watcher keeps working. Users still on v0.9.0's
/// default hotkey are carried to the new Windows-key default once.
pub fn load(app: &AppHandle) -> ClipboardState {
    let file: ClipboardFile = clipboard_path(app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default();
    let cap = file.cap.clamp(MIN_CAP, MAX_CAP);
    let hotkey = migrate_hotkey(&file.hotkey);
    // The Win-key tap is Windows-only; a file carried over from a Windows
    // install must not try to enable it on Linux.
    let win_tap = file.win_tap && cfg!(windows);
    let popup_tab = normalize_popup_tab(&file.popup_tab);
    let popup_pinned_only = file.popup_pinned_only;
    let mut entries: VecDeque<ClipboardEntry> = file.entries.into();
    // Enforce per-kind caps on load too (a hand-edited or future file
    // could exceed them); evicted image PNGs are deleted.
    let doomed = enforce_caps(&mut entries, cap);
    if !doomed.is_empty() {
        if let Ok(dir) = images_dir(app) {
            for f in doomed {
                let _ = fs::remove_file(dir.join(f));
            }
        }
    }
    ClipboardState {
        inner: Mutex::new(ClipboardData {
            entries,
            cap,
            hotkey,
            win_tap,
            popup_tab,
            popup_pinned_only,
            last_seen: None,
            last_image_fp: None,
        }),
    }
}

fn images_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join(IMAGES_DIR))
}

fn persist(app: &AppHandle, data: &ClipboardData) -> Result<(), String> {
    let path = clipboard_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("could not create app data dir: {e}"))?;
    }
    let file = ClipboardFile {
        entries: data.entries.iter().cloned().collect(),
        cap: data.cap,
        hotkey: data.hotkey.clone(),
        win_tap: data.win_tap,
        popup_tab: data.popup_tab.clone(),
        popup_pinned_only: data.popup_pinned_only,
    };
    let json =
        serde_json::to_string_pretty(&file).map_err(|e| format!("could not encode: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| format!("could not write: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("could not save: {e}"))?;
    Ok(())
}

fn with_state<T>(app: &AppHandle, f: impl FnOnce(&mut ClipboardData) -> T) -> Result<T, String> {
    let state = app
        .try_state::<ClipboardState>()
        .ok_or_else(|| "clipboard state not initialized".to_string())?;
    let mut data = state
        .inner
        .lock()
        .map_err(|e| format!("clipboard state poisoned: {e}"))?;
    Ok(f(&mut data))
}

// ---------------------------------------------------------------------------
// Pure recording rules (unit-tested, no AppHandle needed).
// ---------------------------------------------------------------------------

/// Should this clipboard text become a history entry? Skips blanks,
/// oversized texts, and anything identical to what we last saw.
fn should_record(text: &str, last_seen: Option<&str>) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    if text.len() > MAX_TEXT_BYTES {
        return false;
    }
    match last_seen {
        Some(last) => text != last,
        None => true,
    }
}

/// Push a new entry to the front, enforcing the cap. Pure for testing.
fn insert_entry(entries: &mut VecDeque<ClipboardEntry>, entry: ClipboardEntry, cap: usize) {
    entries.push_front(entry);
    entries.truncate(cap.max(1));
}

/// Enforce the text cap and the (fixed) image cap independently on a
/// newest-first deque, keeping the newest entries of each kind. Returns the
/// image file names that were evicted so the caller can delete the PNGs.
/// Pure for testing.
fn enforce_caps(entries: &mut VecDeque<ClipboardEntry>, text_cap: usize) -> Vec<String> {
    let text_cap = text_cap.max(1);
    let mut texts = 0usize;
    let mut images = 0usize;
    let mut doomed = Vec::new();
    entries.retain(|e| {
        let keep = match e.kind {
            EntryKind::Text => {
                texts += 1;
                texts <= text_cap
            }
            EntryKind::Image => {
                images += 1;
                images <= IMAGE_CAP
            }
        };
        if !keep {
            if let Some(f) = &e.image_file {
                doomed.push(f.clone());
            }
        }
        keep
    });
    doomed
}

/// Sampled fingerprint of clipboard image bytes. Hashing a full 33 MiB
/// screenshot every 600 ms tick would burn CPU for nothing — dedupe only
/// needs to notice *change*, and dimensions + length + head/tail samples
/// do that.
fn image_fingerprint(width: u32, height: u32, rgba: &[u8]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    width.hash(&mut h);
    height.hash(&mut h);
    rgba.len().hash(&mut h);
    let n = rgba.len().min(8192);
    rgba[..n].hash(&mut h);
    if rgba.len() > n {
        rgba[rgba.len() - n..].hash(&mut h);
    }
    h.finish()
}

/// Encode raw RGBA8 pixels as a PNG (same pattern as the icon extractor in
/// launcher.rs).
fn encode_png_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc
            .write_header()
            .map_err(|e| format!("could not save the image: {e}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| format!("could not save the image: {e}"))?;
    }
    Ok(buf)
}

/// Decode one of our recorded PNGs back to raw RGBA8. Refuses anything that
/// isn't 8-bit RGBA rather than guessing at a conversion.
fn decode_png_rgba(path: &PathBuf) -> Result<(u32, u32, Vec<u8>), String> {
    let bad = || "That image is in a format I can't paste back.".to_string();
    let file = std::fs::File::open(path).map_err(|_| bad())?;
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().map_err(|_| bad())?;
    let info = reader.info();
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(bad());
    }
    let (w, h) = (info.width, info.height);
    let mut buf = vec![0u8; reader.output_buffer_size()];
    reader.next_frame(&mut buf).map_err(|_| bad())?;
    buf.truncate((w as usize) * (h as usize) * 4);
    Ok((w, h, buf))
}

fn make_entry(text: String) -> ClipboardEntry {
    let n = ENTRY_COUNTER.fetch_add(1, Ordering::Relaxed);
    ClipboardEntry {
        id: format!("clip-{}-{n}", unix_millis()),
        kind: EntryKind::Text,
        text,
        image_file: None,
        width: None,
        height: None,
        bytes: None,
        img_hash: None,
        pinned: false,
        created_at_ms: unix_millis(),
    }
}

fn make_image_entry(image_file: String, width: u32, height: u32, bytes: u64, fp: u64) -> ClipboardEntry {
    let n = ENTRY_COUNTER.fetch_add(1, Ordering::Relaxed);
    ClipboardEntry {
        id: format!("img-{}-{n}", unix_millis()),
        kind: EntryKind::Image,
        text: String::new(),
        image_file: Some(image_file),
        width: Some(width),
        height: Some(height),
        bytes: Some(bytes),
        img_hash: Some(fp),
        pinned: false,
        created_at_ms: unix_millis(),
    }
}

/// Record one clipboard observation. Returns true when a new entry was
/// stored. Extracted so the watcher loop stays thin.
fn observe(app: &AppHandle, text: String) -> bool {
    let recorded = with_state(app, |data| {
        if !should_record(&text, data.last_seen.as_deref()) {
            // Still remember it: an unchanged clipboard must not be
            // re-examined as "new" later.
            data.last_seen = Some(text);
            return false;
        }
        data.last_seen = Some(text.clone());
        insert_entry(&mut data.entries, make_entry(text), data.cap);
        true
    });
    match recorded {
        Ok(true) => {
            let save_result = with_state(app, |data| persist(app, data));
            if let Err(e) = save_result.flatten() {
                eprintln!("clipboard: persist failed: {e}");
            }
            // The popup polls while open (push events proved unreliable for
            // secondary windows), so no emit is needed here.
            true
        }
        Ok(false) => false,
        Err(e) => {
            eprintln!("clipboard: observe failed: {e}");
            false
        }
    }
}

/// Record one clipboard image observation. Returns true when a new entry
/// was stored. The PNG encode + file write happen outside the state lock;
/// the watcher is a single sequential task so nothing races here.
///
/// The image fingerprint is committed only together with the push, AFTER
/// the fallible encode/write steps. Committing it earlier (before knowing
/// the entry is durable) would permanently swallow the image: a failed
/// write would leave the fingerprint claiming "already recorded" and the
/// next tick would skip the retry.
fn observe_image(app: &AppHandle, img: &tauri::image::Image) -> bool {
    let (w, h) = (img.width(), img.height());
    let rgba = img.rgba();
    if w == 0 || h == 0 || rgba.is_empty() || rgba.len() > MAX_IMAGE_BYTES {
        return false;
    }
    let fp = image_fingerprint(w, h, rgba);
    // Check only for now — the commit happens with the push below.
    match with_state(app, |data| data.last_image_fp == Some(fp)) {
        Ok(true) => return false,
        Err(e) => {
            eprintln!("clipboard: image observe failed: {e}");
            return false;
        }
        Ok(false) => {}
    }
    let file_name = format!("img-{}-{}.png", unix_millis(), ENTRY_COUNTER.load(Ordering::Relaxed));
    let png = match encode_png_rgba(w, h, rgba) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("clipboard: {e}");
            return false;
        }
    };
    let dir = match images_dir(app) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("clipboard: {e}");
            return false;
        }
    };
    if let Err(e) = fs::create_dir_all(&dir).and_then(|_| fs::write(dir.join(&file_name), &png)) {
        eprintln!("clipboard: could not save the image: {e}");
        return false;
    }
    let entry = make_image_entry(file_name.clone(), w, h, rgba.len() as u64, fp);
    let saved = with_state(app, |data| {
        if data.last_image_fp == Some(fp) {
            // Raced with a copy-back that already accounted for this
            // image; drop the orphaned PNG, keep the existing entry.
            return (Vec::new(), Some(file_name.clone()));
        }
        data.last_image_fp = Some(fp);
        data.entries.push_front(entry);
        let doomed = enforce_caps(&mut data.entries, data.cap);
        (doomed, None)
    });
    // Evicted entries' PNGs are deleted whether or not the JSON persist
    // below succeeds — they're already out of the deque either way.
    let (doomed, orphan) = match saved {
        Ok(v) => v,
        Err(e) => {
            eprintln!("clipboard: image observe failed: {e}");
            // State lock failed after the PNG was written: the fingerprint
            // was NOT committed, so the next tick retries the image
            // instead of dropping it. Remove the orphaned file.
            let _ = fs::remove_file(dir.join(&file_name));
            return false;
        }
    };
    let persist_result = with_state(app, |data| persist(app, data));
    match persist_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) | Err(e) => eprintln!("clipboard: persist failed: {e}"),
    }
    for f in doomed {
        let _ = fs::remove_file(dir.join(f));
    }
    if let Some(f) = orphan {
        let _ = fs::remove_file(dir.join(f));
    }
    true
}

/// Read an image from the OS clipboard, beyond what the clipboard plugin
/// sees (v0.9.2). On Windows the DIB is read directly — CF_DIBV5, then
/// plain CF_DIB (what Win+Shift+S / the Snipping Tool place), then the
/// registered "PNG" format — because the plugin's read_image missed a
/// real screenshot on the user's PC. Everywhere else the plugin reads
/// image/png first; on Linux image/bmp is tried next (arboard never
/// requests that target).
fn read_os_image(app: &AppHandle) -> Result<tauri::image::Image<'_>, String> {
    #[cfg(windows)]
    {
        let _ = app;
        let img = crate::clipboard_img::read_windows_image()?;
        Ok(tauri::image::Image::new_owned(
            img.rgba,
            img.width,
            img.height,
        ))
    }
    #[cfg(not(windows))]
    {
        match app.clipboard().read_image() {
            Ok(img) => Ok(img),
            #[cfg(target_os = "linux")]
            Err(_) => {
                let bmp = crate::clipboard_img::read_linux_image_bmp()?;
                Ok(tauri::image::Image::new_owned(
                    bmp.rgba,
                    bmp.width,
                    bmp.height,
                ))
            }
            #[cfg(not(target_os = "linux"))]
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Background watcher: one text read + compare and one image probe per
/// tick. Started once from main.rs setup.
pub fn start_watcher(app: AppHandle) {
    // Seed both dedupe memories from whatever is already on the clipboard
    // so pre-existing content isn't recorded as new history.
    if let Ok(current) = app.clipboard().read_text() {
        let _ = with_state(&app, |data| {
            data.last_seen = Some(current);
        });
    }
    if let Ok(img) = read_os_image(&app) {
        let fp = image_fingerprint(img.width(), img.height(), img.rgba());
        let _ = with_state(&app, |data| {
            data.last_image_fp = Some(fp);
        });
    }
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        loop {
            interval.tick().await;
            match app.clipboard().read_text() {
                Ok(text) => {
                    observe(&app, text);
                }
                Err(_) => {
                    // Clipboard unavailable right now (locked by another
                    // app, no text format, X11 owner gone). Next tick.
                }
            }
            // Image probe: fails fast when the clipboard holds no image.
            if let Ok(img) = read_os_image(&app) {
                observe_image(&app, &img);
            }
        }
    });
}

/// Register the saved popup hotkey at startup. Best-effort: a failure is
/// recorded in the binding status (the UI warns) and logged — startup
/// never crashes on a hotkey.
pub fn register_saved_hotkey(app: &AppHandle) {
    let hotkey = with_state(app, |data| data.hotkey.clone()).unwrap_or_default();
    let hotkey = hotkey.trim().to_string();
    if hotkey.is_empty() {
        return;
    }
    if let Err(e) = set_binding(app, BINDING_ID, HotkeyKind::Clipboard, &hotkey) {
        eprintln!("clipboard hotkey: {e}");
        record_binding_failure(app, BINDING_ID, hotkey, e);
    }
}

/// Startup: summon via the bare-Windows-key tap when the user opted in
/// (Windows only), otherwise register the saved combo hotkey.
pub fn ensure_summon_registered(app: &AppHandle) {
    #[cfg(windows)]
    {
        let win_tap = with_state(app, |data| data.win_tap).unwrap_or(false);
        if win_tap {
            if let Err(e) = crate::winkey::install(app) {
                eprintln!("windows-key summon: {e}");
            }
            return;
        }
    }
    register_saved_hotkey(app);
}

// ---------------------------------------------------------------------------
// Popup window.
// ---------------------------------------------------------------------------

/// Clamp a cursor-anchored popup origin inside a monitor rectangle.
/// Panic-free even when the window is bigger than the monitor.
/// Pure math — unit-tested.
fn clamp_popup_origin(
    cursor: (i32, i32),
    monitor: (i32, i32, i32, i32),
    window: (i32, i32),
) -> (i32, i32) {
    let (cursor_x, cursor_y) = cursor;
    let (mon_x, mon_y, mon_w, mon_h) = monitor;
    let (win_w, win_h) = window;
    let max_x = (mon_x + mon_w - win_w).max(mon_x);
    let max_y = (mon_y + mon_h - win_h).max(mon_y);
    let x = (cursor_x + 12).clamp(mon_x, max_x);
    let y = (cursor_y + 12).clamp(mon_y, max_y);
    (x, y)
}

fn position_clipboard_window(app: &AppHandle, w: &tauri::WebviewWindow) {
    use tauri::{PhysicalPosition, Position};
    const WIN_W: i32 = 440;
    const WIN_H: i32 = 520;
    if let Some(monitor) = crate::windows::cursor_monitor(app) {
        let mp = monitor.position();
        let ms = monitor.size();
        let (cx, cy) = app
            .cursor_position()
            .ok()
            .map(|p| (p.x as i32, p.y as i32))
            .unwrap_or((mp.x, mp.y));
        let (x, y) = clamp_popup_origin(
            (cx, cy),
            (mp.x, mp.y, ms.width as i32, ms.height as i32),
            (WIN_W, WIN_H),
        );
        if w
            .set_position(Position::Physical(PhysicalPosition::new(x, y)))
            .is_ok()
        {
            return;
        }
    }
    let _ = w.center();
}

fn build_clipboard_window(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window(WINDOW_LABEL).is_some() {
        return Ok(());
    }
    let win = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("clipboard.html".into()))
        .title("Clipboard history")
        .inner_size(440.0, 520.0)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(true)
        .build()
        .map_err(|e| format!("could not open clipboard history: {e}"))?;
    position_clipboard_window(app, &win);
    let _ = win.show();
    let _ = win.set_focus();
    Ok(())
}

/// Toggle the clipboard popup. Called from the global-shortcut handler in
/// main.rs (NOT a Tauri command): window creation happens on a dedicated
/// thread, never on a sync command thread or the main thread.
pub fn toggle_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(WINDOW_LABEL) {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            position_clipboard_window(app, &w);
            let _ = w.show();
            let _ = w.set_focus();
        }
        return;
    }
    let handle = app.clone();
    std::thread::Builder::new()
        .name("appmaka-clipboard-window".to_string())
        .spawn(move || {
            if let Err(e) = build_clipboard_window(&handle) {
                eprintln!("clipboard window: {e}");
            }
        })
        .ok();
}

// ---------------------------------------------------------------------------
// Tauri commands. JS: invoke("list_clipboard") etc. (camelCase args).
// ---------------------------------------------------------------------------

/// Hide the popup. The frontend calls this instead of window.hide() from JS:
/// the Rust-side hide is the path proven to work for this window.
/// JS: `invoke("hide_clipboard_popup")`.
#[tauri::command]
pub fn hide_clipboard_popup(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(WINDOW_LABEL) {
        w.hide().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Newest-first history. Texts are truncated to previews; images expose
/// an absolute PNG path for the UI thumbnail.
/// JS: `invoke("list_clipboard")`.
#[tauri::command]
pub fn list_clipboard(app: AppHandle) -> Result<Vec<ClipboardListEntry>, String> {
    let dir = images_dir(&app).ok();
    with_state(&app, |data| {
        data.entries
            .iter()
            .map(|e| {
                let (preview, chars, truncated) = match e.kind {
                    EntryKind::Text => {
                        let chars = e.text.chars().count();
                        let truncated = chars > PREVIEW_CHARS;
                        let preview: String = if truncated {
                            e.text.chars().take(PREVIEW_CHARS).collect()
                        } else {
                            e.text.clone()
                        };
                        (preview, chars, truncated)
                    }
                    EntryKind::Image => (String::new(), 0, false),
                };
                let image_path = match (&e.kind, &e.image_file, &dir) {
                    (EntryKind::Image, Some(f), Some(d)) => {
                        Some(d.join(f).to_string_lossy().into_owned())
                    }
                    _ => None,
                };
                ClipboardListEntry {
                    id: e.id.clone(),
                    kind: match e.kind {
                        EntryKind::Text => "text".to_string(),
                        EntryKind::Image => "image".to_string(),
                    },
                    preview,
                    chars,
                    truncated,
                    pinned: e.pinned,
                    created_at_ms: e.created_at_ms,
                    image_path,
                    width: e.width,
                    height: e.height,
                }
            })
            .collect()
    })
}

/// Copy an entry back to the OS clipboard (v2 contract: the user pastes
/// from there; we don't synthesize keystrokes into other apps).
/// JS: `invoke("copy_clipboard_entry", { entryId })`.
#[tauri::command]
pub fn copy_clipboard_entry(app: AppHandle, entry_id: String) -> Result<(), String> {
    let entry = with_state(&app, |data| {
        data.entries.iter().find(|e| e.id == entry_id).cloned()
    })?
    .ok_or_else(|| "That clipboard entry is gone.".to_string())?;
    match entry.kind {
        EntryKind::Text => {
            app.clipboard()
                .write_text(entry.text.clone())
                .map_err(|e| format!("Couldn't write to the clipboard: {e}"))?;
            // Remember what we just wrote so the watcher doesn't
            // re-record it as a new copy.
            let _ = with_state(&app, |data| {
                data.last_seen = Some(entry.text);
            });
        }
        EntryKind::Image => {
            let file = entry
                .image_file
                .clone()
                .ok_or_else(|| "That image is gone.".to_string())?;
            let (w, h, rgba) = decode_png_rgba(&images_dir(&app)?.join(file))?;
            let img = tauri::image::Image::new_owned(rgba, w, h);
            app.clipboard()
                .write_image(&img)
                .map_err(|e| format!("Couldn't write to the clipboard: {e}"))?;
            let _ = with_state(&app, |data| {
                data.last_image_fp = entry.img_hash;
            });
        }
    }
    Ok(())
}

/// Merge several text entries into one clipboard payload (v0.9.3
/// multi-select). Pure: ids arrive in display order (newest first) and the
/// join preserves it. Images are excluded — the OS clipboard holds one
/// image via write_image, so combining them is meaningless; the UI keeps
/// images out of multi-select mode. Unknown ids are skipped (an entry can
/// be evicted between list and copy).
fn merge_selected_texts(
    entries: &VecDeque<ClipboardEntry>,
    ids: &[String],
) -> Result<(String, usize), String> {
    let texts: Vec<&str> = ids
        .iter()
        .filter_map(|id| entries.iter().find(|e| &e.id == id))
        .filter(|e| e.kind == EntryKind::Text)
        .map(|e| e.text.as_str())
        .collect();
    if texts.is_empty() {
        return Err("Nothing to copy.".to_string());
    }
    let count = texts.len();
    Ok((texts.join("\n\n"), count))
}

/// Copy several text entries as ONE clipboard payload (v0.9.3 multi-select).
/// `entry_ids` arrive in display order (newest first); texts are joined
/// with blank lines. Images are skipped — see `merge_selected_texts`.
/// JS: `invoke("copy_clipboard_entries", { entryIds })`.
#[tauri::command]
pub fn copy_clipboard_entries(
    app: AppHandle,
    entry_ids: Vec<String>,
) -> Result<usize, String> {
    let (joined, count) =
        with_state(&app, |data| merge_selected_texts(&data.entries, &entry_ids))??;
    app.clipboard()
        .write_text(joined.clone())
        .map_err(|e| format!("Couldn't write to the clipboard: {e}"))?;
    // Remember what we just wrote so the watcher doesn't re-record the
    // merged payload as a new copy.
    let _ = with_state(&app, |data| {
        data.last_seen = Some(joined);
    });
    Ok(count)
}

/// Empty the history (text + images, including the saved PNGs). The OS
/// clipboard is untouched.
/// JS: `invoke("clear_clipboard")`.
#[tauri::command]
pub fn clear_clipboard(app: AppHandle) -> Result<(), String> {
    let dir = images_dir(&app).ok();
    with_state(&app, |data| {
        data.entries.clear();
        data.last_seen = None;
        data.last_image_fp = None;
        persist(&app, data)
    })??;
    if let Some(d) = dir {
        // Best-effort: a failed wipe must not fail the clear.
        let _ = fs::remove_dir_all(d);
    }
    Ok(())
}

/// Current cap + hotkey + summon mode for the Settings UI.
/// JS: `invoke("get_clipboard_settings")`.
#[tauri::command]
pub fn get_clipboard_settings(app: AppHandle) -> Result<ClipboardSettings, String> {
    with_state(&app, |data| ClipboardSettings {
        cap: data.cap,
        hotkey: data.hotkey.clone(),
        win_tap: data.win_tap,
        win_tap_supported: cfg!(windows),
        popup_tab: data.popup_tab.clone(),
        popup_pinned_only: data.popup_pinned_only,
    })
}

/// Opt into (or out of) summoning the popup by tapping the bare Windows
/// key (v0.9.2, Windows only). Enabling installs the low-level hook and
/// retires the combo binding — leaving both live would toggle the popup
/// twice per press. Disabling uninstalls the hook and re-registers the
/// saved combo. The hook is installed only while this is on.
/// JS: `invoke("set_clipboard_win_tap", { enabled })`.
#[tauri::command]
pub fn set_clipboard_win_tap(app: AppHandle, enabled: bool) -> Result<bool, String> {
    #[cfg(windows)]
    {
        if enabled {
            // Install first; only persist once the hook is actually live.
            crate::winkey::install(&app)?;
            let _ = remove_binding(&app, BINDING_ID);
            with_state(&app, |data| {
                data.win_tap = true;
                persist(&app, data)
            })??;
        } else {
            crate::winkey::uninstall();
            with_state(&app, |data| {
                data.win_tap = false;
                persist(&app, data)
            })??;
            register_saved_hotkey(&app);
        }
        Ok(enabled)
    }
    #[cfg(not(windows))]
    {
        let _ = &app;
        let _ = enabled;
        Err("Tapping the Windows key to open the clipboard only works on Windows.".to_string())
    }
}

/// Remember the popup's tab ("all" | "text" | "image") across summons and
/// restarts (v0.9.4). Unknown values normalize to "all" rather than
/// erroring — the popup only ever sends the three it renders.
/// JS: `invoke("set_clipboard_popup_tab", { tab })`.
#[tauri::command]
pub fn set_clipboard_popup_tab(app: AppHandle, tab: String) -> Result<String, String> {
    let tab = normalize_popup_tab(&tab);
    with_state(&app, |data| {
        data.popup_tab = tab.clone();
        persist(&app, data).map(|_| tab.clone())
    })?
}

/// Pin or unpin one clipboard entry (v0.9.12). The pin is persisted; an
/// entry evicted by the caps simply disappears, pin and all.
/// JS: `invoke("set_clipboard_pinned", { entryId, pinned })`.
#[tauri::command]
pub fn set_clipboard_pinned(
    app: AppHandle,
    entry_id: String,
    pinned: bool,
) -> Result<bool, String> {
    with_state(&app, |data| {
        if !apply_pin(&mut data.entries, &entry_id, pinned) {
            return Err("That clipboard entry is gone.".to_string());
        }
        persist(&app, data).map(|_| pinned)
    })?
}

/// Remember the popup's "Pinned only" filter across summons and restarts
/// (v0.9.12) — same persistence contract as the tab.
/// JS: `invoke("set_clipboard_pinned_only", { pinnedOnly })`.
#[tauri::command]
pub fn set_clipboard_pinned_only(
    app: AppHandle,
    pinned_only: bool,
) -> Result<bool, String> {
    with_state(&app, |data| {
        data.popup_pinned_only = pinned_only;
        persist(&app, data).map(|_| pinned_only)
    })?
}

/// Set or clear the pin on one entry. Pure for testing: true when the
/// entry existed.
fn apply_pin(entries: &mut VecDeque<ClipboardEntry>, id: &str, pinned: bool) -> bool {
    match entries.iter_mut().find(|e| e.id == id) {
        Some(e) => {
            e.pinned = pinned;
            true
        }
        None => false,
    }
}

/// Change the text history cap (10–1000). Truncates text entries
/// immediately; image entries keep their own fixed cap of 25.
/// JS: `invoke("set_clipboard_cap", { cap })`.
#[tauri::command]
pub fn set_clipboard_cap(app: AppHandle, cap: usize) -> Result<usize, String> {
    if !(MIN_CAP..=MAX_CAP).contains(&cap) {
        return Err(format!("Keep the history size between {MIN_CAP} and {MAX_CAP}."));
    }
    let dir = images_dir(&app).ok();
    let doomed = with_state(&app, |data| {
        data.cap = cap;
        let doomed = enforce_caps(&mut data.entries, cap);
        persist(&app, data).map(|_| doomed)
    })??;
    if let Some(d) = dir {
        for f in doomed {
            let _ = fs::remove_file(d.join(f));
        }
    }
    Ok(cap)
}

/// Change the popup hotkey. Goes through the shared registry so conflicts
/// (another AppMaka shortcut, or the launcher summon key) come back as
/// plain-language errors. Empty clears the binding.
/// JS: `invoke("set_clipboard_hotkey", { hotkey })`.
#[tauri::command]
pub fn set_clipboard_hotkey(app: AppHandle, hotkey: String) -> Result<String, String> {
    let normalized = hotkey.trim().to_string();
    set_binding(&app, BINDING_ID, HotkeyKind::Clipboard, &normalized)?;
    with_state(&app, |data| {
        data.hotkey = normalized.clone();
        persist(&app, data).map(|_| normalized)
    })?
}

/// Registration status of the popup hotkey, for the conflict warning UI.
/// JS: `invoke("clipboard_hotkey_status")`.
#[tauri::command]
pub fn clipboard_hotkey_status(app: AppHandle) -> Result<BindingStatus, String> {
    if let Some(status) = crate::hotkeys::binding_status(&app, BINDING_ID) {
        return Ok(status);
    }
    let hotkey = with_state(&app, |data| data.hotkey.clone()).unwrap_or_default();
    Ok(BindingStatus {
        hotkey,
        registered: false,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_record_rules() {
        assert!(!should_record("", None));
        assert!(!should_record("   \n  ", None));
        assert!(!should_record("hello", Some("hello")));
        assert!(should_record("hello", Some("bye")));
        assert!(should_record("hello", None));
        let big = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(!should_record(&big, None));
        let exactly = "x".repeat(MAX_TEXT_BYTES);
        assert!(should_record(&exactly, None));
    }

    #[test]
    fn insert_entry_pushes_front_and_caps() {
        let mut entries = VecDeque::new();
        for i in 0..5 {
            insert_entry(
                &mut entries,
                ClipboardEntry {
                    id: format!("clip-{i}"),
                    kind: EntryKind::Text,
                    text: format!("text {i}"),
                    image_file: None,
                    width: None,
                    height: None,
                    bytes: None,
                    img_hash: None,
                    pinned: false,
                    created_at_ms: i,
                },
                3,
            );
        }
        assert_eq!(entries.len(), 3);
        // Newest first.
        assert_eq!(entries[0].id, "clip-4");
        assert_eq!(entries[2].id, "clip-2");
    }

    fn text_entry(id: &str) -> ClipboardEntry {
        ClipboardEntry {
            id: id.to_string(),
            kind: EntryKind::Text,
            text: "t".to_string(),
            image_file: None,
            width: None,
            height: None,
            bytes: None,
            img_hash: None,
            pinned: false,
            created_at_ms: 0,
        }
    }

    fn image_entry(id: &str) -> ClipboardEntry {
        ClipboardEntry {
            id: id.to_string(),
            kind: EntryKind::Image,
            text: String::new(),
            image_file: Some(format!("{id}.png")),
            width: Some(100),
            height: Some(100),
            bytes: Some(40000),
            img_hash: Some(1),
            pinned: false,
            created_at_ms: 0,
        }
    }

    #[test]
    fn enforce_caps_keeps_newest_of_each_kind() {
        let mut entries = VecDeque::new();
        for i in 0..5 {
            entries.push_front(text_entry(&format!("t{i}")));
        }
        for i in 0..30 {
            entries.push_front(image_entry(&format!("i{i}")));
        }
        // Mixed order, newest first: interleave to prove per-kind counting.
        let mut mixed = VecDeque::new();
        for i in 0..5 {
            mixed.push_front(text_entry(&format!("t{i}")));
            mixed.push_front(image_entry(&format!("i{i}")));
        }
        let doomed = enforce_caps(&mut mixed, 3);
        // 3 newest texts kept (t4, t3, t2); all 5 images fit in the 25 cap.
        let texts: Vec<_> = mixed
            .iter()
            .filter(|e| e.kind == EntryKind::Text)
            .collect();
        assert_eq!(texts.len(), 3);
        assert_eq!(texts[0].id, "t4");
        assert_eq!(texts[2].id, "t2");
        assert_eq!(
            mixed.iter().filter(|e| e.kind == EntryKind::Image).count(),
            5
        );
        // Evicted texts had no PNG files, so nothing to delete.
        assert!(doomed.is_empty());

        // Image cap: 30 images, only 25 survive, evicted PNGs reported.
        let doomed = enforce_caps(&mut entries, 100);
        let images: Vec<_> = entries
            .iter()
            .filter(|e| e.kind == EntryKind::Image)
            .collect();
        assert_eq!(images.len(), IMAGE_CAP);
        assert_eq!(images[0].id, "i29");
        assert_eq!(doomed.len(), 5);
        assert!(doomed.contains(&"i0.png".to_string()));
        assert!(doomed.contains(&"i4.png".to_string()));
        assert!(!doomed.contains(&"i5.png".to_string()));
        // All 5 texts survive a text cap of 100.
        assert_eq!(
            entries.iter().filter(|e| e.kind == EntryKind::Text).count(),
            5
        );
    }

    #[test]
    fn image_fingerprint_notices_change() {
        let a = vec![1u8; 100_000];
        let b = vec![1u8; 100_000];
        let mut c = vec![1u8; 100_000];
        c[99_999] = 2; // tail sample differs
        let mut d = vec![1u8; 100_000];
        d[0] = 2; // head sample differs
        assert_eq!(image_fingerprint(100, 250, &a), image_fingerprint(100, 250, &b));
        assert_ne!(image_fingerprint(100, 250, &a), image_fingerprint(100, 250, &c));
        assert_ne!(image_fingerprint(100, 250, &a), image_fingerprint(100, 250, &d));
        // Dimensions and length are part of the fingerprint.
        assert_ne!(image_fingerprint(100, 250, &a), image_fingerprint(200, 125, &a));
        assert_ne!(
            image_fingerprint(100, 250, &a),
            image_fingerprint(100, 250, &a[..50_000])
        );
    }

    #[test]
    fn new_default_hotkey_uses_windows_key() {
        // Win+Shift+V is reserved (shell notification cycling); the default
        // must stay on the Windows key without colliding with it.
        assert_eq!(DEFAULT_HOTKEY, "Super+Alt+V");
        assert!(crate::hotkeys::validate_hotkey_syntax(DEFAULT_HOTKEY).is_ok());
    }

    #[test]
    fn hotkey_migration_carries_old_default() {
        assert_eq!(migrate_hotkey("Ctrl+Shift+V"), "Super+Alt+V");
        // Deliberate user choices are untouched.
        assert_eq!(migrate_hotkey("Ctrl+Alt+X"), "Ctrl+Alt+X");
        assert_eq!(migrate_hotkey(""), "");
    }

    #[test]
    fn clamp_popup_origin_cases() {
        // Cursor-anchored with 12px offset, inside a 1920x1080 monitor.
        assert_eq!(
            clamp_popup_origin((100, 100), (0, 0, 1920, 1080), (440, 520)),
            (112, 112)
        );
        // Near the right edge: clamped so the window stays on-screen.
        assert_eq!(
            clamp_popup_origin((1900, 100), (0, 0, 1920, 1080), (440, 520)),
            (1480, 112)
        );
        // Near the bottom edge: clamped upward (600 - 520 = 80).
        assert_eq!(
            clamp_popup_origin((100, 580), (0, 0, 1920, 600), (440, 520)),
            (112, 80)
        );
        // Monitor with a negative origin (multi-monitor X11).
        assert_eq!(
            clamp_popup_origin((-1900, 100), (-1920, 0, 1920, 1080), (440, 520)),
            (-1888, 112)
        );
        // Window taller than the monitor: pinned to the monitor origin,
        // never panics.
        assert_eq!(
            clamp_popup_origin((100, 100), (0, 0, 300, 200), (440, 520)),
            (0, 0)
        );
    }

    #[test]
    fn file_round_trip_with_defaults() {
        // Old/minimal files deserialize through defaults.
        let f: ClipboardFile = serde_json::from_str("{}").unwrap();
        assert_eq!(f.cap, DEFAULT_CAP);
        assert_eq!(f.hotkey, DEFAULT_HOTKEY);
        assert!(f.entries.is_empty());
        assert!(!f.win_tap);

        // A v0.9.0 text entry (no kind field) loads as Text.
        let old: ClipboardEntry =
            serde_json::from_str(r#"{"id":"clip-9","text":"hi","createdAtMs":42}"#).unwrap();
        assert_eq!(old.kind, EntryKind::Text);
        assert_eq!(old.text, "hi");
        assert_eq!(old.image_file, None);

        let full = ClipboardFile {
            entries: vec![ClipboardEntry {
                id: "clip-1".to_string(),
                kind: EntryKind::Text,
                text: "hi".to_string(),
                image_file: None,
                width: None,
                height: None,
                bytes: None,
                img_hash: None,
                pinned: false,
                created_at_ms: 42,
            }],
            cap: 50,
            hotkey: "Ctrl+Shift+X".to_string(),
            win_tap: true,
            popup_tab: "image".to_string(),
            popup_pinned_only: false,
        };
        let encoded = serde_json::to_string(&full).unwrap();
        let decoded: ClipboardFile = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(decoded.entries[0].text, "hi");
        assert_eq!(decoded.cap, 50);
        assert_eq!(decoded.hotkey, "Ctrl+Shift+X");
        assert!(decoded.win_tap);
        assert_eq!(decoded.popup_tab, "image");
    }

    #[test]
    fn popup_tab_normalizes_and_defaults_to_all() {
        assert_eq!(normalize_popup_tab("all"), "all");
        assert_eq!(normalize_popup_tab("text"), "text");
        assert_eq!(normalize_popup_tab("image"), "image");
        // Unknown, empty, or oddly-cased values fall back to the mixed
        // list rather than breaking the popup.
        assert_eq!(normalize_popup_tab(""), "all");
        assert_eq!(normalize_popup_tab("bogus"), "all");
        assert_eq!(normalize_popup_tab("Text"), "all");

        // Old files without the field deserialize through the default.
        let f: ClipboardFile = serde_json::from_str("{}").unwrap();
        assert_eq!(f.popup_tab, "all");
        // camelCase on the wire, like the rest of the file.
        let f: ClipboardFile =
            serde_json::from_str(r#"{"popupTab":"text"}"#).unwrap();
        assert_eq!(f.popup_tab, "text");
        // A bad stored value normalizes on load, not just on set.
        assert_eq!(normalize_popup_tab(&f.popup_tab), "text");
        let f: ClipboardFile =
            serde_json::from_str(r#"{"popupTab":"nope"}"#).unwrap();
        assert_eq!(normalize_popup_tab(&f.popup_tab), "all");
    }

    #[test]
    fn merge_selected_preserves_order_and_joins_with_blank_lines() {
        let mut entries = VecDeque::new();
        // Display order is newest first; ids arrive in that order.
        for (id, text) in [("c1", "first"), ("c2", "second"), ("c3", "third")] {
            let mut e = text_entry(id);
            e.text = text.to_string();
            entries.push_back(e);
        }
        let ids = vec!["c1".to_string(), "c2".to_string(), "c3".to_string()];
        let (joined, count) = merge_selected_texts(&entries, &ids).unwrap();
        assert_eq!(joined, "first\n\nsecond\n\nthird");
        assert_eq!(count, 3);
    }

    #[test]
    fn merge_selected_excludes_images_and_skips_unknown_ids() {
        let mut entries = VecDeque::new();
        let mut t1 = text_entry("t1");
        t1.text = "keep me".to_string();
        entries.push_back(t1);
        entries.push_back(image_entry("i1"));
        let mut t2 = text_entry("t2");
        t2.text = "me too".to_string();
        entries.push_back(t2);
        // Image id + unknown id are skipped, not errors.
        let ids = vec![
            "t1".to_string(),
            "i1".to_string(),
            "gone".to_string(),
            "t2".to_string(),
        ];
        let (joined, count) = merge_selected_texts(&entries, &ids).unwrap();
        assert_eq!(joined, "keep me\n\nme too");
        assert_eq!(count, 2);
    }

    #[test]
    fn merge_selected_empty_is_an_error() {
        let entries = VecDeque::new();
        assert!(merge_selected_texts(&entries, &[]).is_err());
        // Images alone are not copyable as a merge.
        let mut only_image = VecDeque::new();
        only_image.push_back(image_entry("i1"));
        assert!(merge_selected_texts(&only_image, &["i1".to_string()]).is_err());
    }

    #[test]
    fn apply_pin_sets_clears_and_reports_missing() {
        let mut entries = VecDeque::new();
        entries.push_back(text_entry("t1"));
        entries.push_back(image_entry("i1"));

        assert!(apply_pin(&mut entries, "t1", true));
        assert!(entries.iter().find(|e| e.id == "t1").unwrap().pinned);
        // Other entries untouched.
        assert!(!entries.iter().find(|e| e.id == "i1").unwrap().pinned);

        assert!(apply_pin(&mut entries, "t1", false));
        assert!(!entries.iter().find(|e| e.id == "t1").unwrap().pinned);

        // Unknown id: no change, reported as missing.
        assert!(!apply_pin(&mut entries, "gone", true));
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn pin_round_trip_and_legacy_default() {
        // Legacy entries (no `pinned` field) load unpinned.
        let legacy: ClipboardEntry =
            serde_json::from_str(r#"{"id":"clip-9","text":"hi","createdAtMs":42}"#).unwrap();
        assert!(!legacy.pinned);

        // A pinned entry survives a serialize/deserialize round trip.
        let mut pinned = text_entry("p1");
        pinned.pinned = true;
        let encoded = serde_json::to_string(&pinned).unwrap();
        assert!(encoded.contains("\"pinned\":true"));
        let decoded: ClipboardEntry = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.pinned);

        // The filter flag round-trips and defaults off for old files.
        let f: ClipboardFile = serde_json::from_str("{}").unwrap();
        assert!(!f.popup_pinned_only);
        let f: ClipboardFile =
            serde_json::from_str(r#"{"popupPinnedOnly":true}"#).unwrap();
        assert!(f.popup_pinned_only);
        let full = ClipboardFile {
            entries: vec![pinned],
            cap: DEFAULT_CAP,
            hotkey: DEFAULT_HOTKEY.to_string(),
            win_tap: false,
            popup_tab: default_popup_tab(),
            popup_pinned_only: true,
        };
        let decoded: ClipboardFile =
            serde_json::from_str(&serde_json::to_string(&full).unwrap()).unwrap();
        assert!(decoded.popup_pinned_only);
        assert!(decoded.entries[0].pinned);
    }
}
