//! Copyable error dialogs (v0.11.0).
//!
//! A central recent-errors ring buffer. The rule for surfacing:
//!
//! - **Backend-initiated failures** (no frontend invoke in flight — the
//!   user would otherwise see nothing): recorded AND surfaced as a real
//!   dialog via the `appmaka:error` event. The dialog shows a
//!   plain-language message, expandable technical details, and a Copy
//!   button.
//! - **Frontend-initiated invoke errors**: recorded only — their inline
//!   surface (form error, updater panel, downloads page…) already shows.
//!
//! Every entry is sanitized before storage so copied diagnostics can never
//! leak credential or session values. The launcher settings' "Copy
//! diagnostics" action copies recent errors + version + OS for pasting
//! straight into chat. The ring also persists to `<app_data>/errors.json`
//! (v0.16.3): a crash no longer takes the evidence with it — relaunching
//! restores the previous session's errors for copying.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_clipboard_manager::ClipboardExt;

/// Ring capacity: enough history to be useful, bounded for RAM.
const CAP: usize = 64;

/// Keys whose `key=value` / `key: value` occurrences are redacted before
/// storage. Longest first so `api_key` wins over any shorter overlap.
const SECRET_KEYS: &[&str] = &[
    "private_key",
    "privatekey",
    "session_key",
    "sessionkey",
    "api_key",
    "apikey",
    "api-key",
    "password",
    "passwd",
    "secret",
    "bearer",
    "token",
];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ErrorEntry {
    pub id: u64,
    pub unix_secs: u64,
    pub kind: String,
    pub message: String,
    /// Sanitized at record time — safe to display and copy.
    pub technical: String,
}

#[derive(Default)]
pub struct ErrorLog {
    entries: VecDeque<ErrorEntry>,
    next_id: u64,
    /// Where the log persists (`<app_data>/errors.json`), set by
    /// `init_log`. None until then — early-startup errors stay
    /// memory-only rather than failing.
    path: Option<PathBuf>,
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Serialize the ring for disk: a plain JSON array, oldest first —
/// the same order `entries` keeps in memory.
fn serialize_entries(entries: &VecDeque<ErrorEntry>) -> String {
    serde_json::to_string(&entries.iter().collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".to_string())
}

/// Parse a persisted log back. The file is user-writable, so it is
/// never trusted: unparseable input yields an empty log, technical
/// details are re-sanitized, and the ring cap keeps the newest CAP.
fn parse_entries(text: &str) -> VecDeque<ErrorEntry> {
    let mut parsed: Vec<ErrorEntry> = serde_json::from_str(text).unwrap_or_default();
    for e in &mut parsed {
        e.technical = sanitize(&e.technical);
    }
    if parsed.len() > CAP {
        parsed.drain(..parsed.len() - CAP);
    }
    parsed.into()
}

/// Rewrite the persisted log (temp file + rename, the store.rs
/// pattern, so a crash mid-write can't corrupt the previous copy).
/// Best-effort: diagnostics must never fail the operation that was
/// being recorded.
fn persist(log: &ErrorLog) {
    let Some(path) = &log.path else { return };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serialize_entries(&log.entries)).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Load the persisted log and remember its path so `record` keeps the
/// file current. Called once from setup, after the log state is
/// managed. This is what lets a tester relaunch after a crash and
/// still copy the diagnostics that explain it (v0.16.3).
pub fn init_log<R: Runtime>(app: AppHandle<R>) {
    let Ok(dir) = app.path().app_data_dir() else {
        return;
    };
    let path = dir.join("errors.json");
    let Some(state) = app.try_state::<Mutex<ErrorLog>>() else {
        return;
    };
    let Ok(mut log) = state.lock() else { return };
    if let Ok(text) = std::fs::read_to_string(&path) {
        log.entries = parse_entries(&text);
        // record() pre-increments, so next_id is the last id used:
        // restoring it to the max persisted id keeps ids monotonic.
        log.next_id = log.entries.iter().map(|e| e.id).max().unwrap_or(0);
    }
    log.path = Some(path);
}

/// `2026-10-07 02:11:04 UTC` from unix seconds. Hand-rolled civil date
/// (no date crate in the tree); leap years handled, no timezones.
fn format_unix_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        tod / 3600,
        tod % 3600 / 60,
        tod % 60
    )
}

/// Redact `secret_key=value` / `secret_key: value` occurrences
/// (case-insensitive). The value runs to the next whitespace, quote,
/// comma, semicolon, or bracket. Surrounding quotes are preserved.
/// Byte-level and UTF-8 safe: keys and separators are pure ASCII, and
/// non-ASCII bytes are copied as whole chars.
pub fn sanitize(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii() {
            // Keys are ASCII-only; copy one full char and move on.
            let ch = raw[i..].chars().next().unwrap_or('\u{FFFD}');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let key_len = SECRET_KEYS.iter().find_map(|k| {
            let kb = k.as_bytes();
            (i + kb.len() <= bytes.len() && bytes[i..i + kb.len()].eq_ignore_ascii_case(kb))
                .then_some(kb.len())
        });
        let key_len = match key_len {
            Some(l) => l,
            None => {
                out.push(bytes[i] as char);
                i += 1;
                continue;
            }
        };
        // After the key: optional spaces, then ':' or '=', then the value.
        let mut j = i + key_len;
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        if j >= bytes.len() || (bytes[j] != b':' && bytes[j] != b'=') {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        j += 1;
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        // i..j is ASCII-only (key + spaces + separator): safe to slice.
        out.push_str(&raw[i..j]);
        // Quoted value: redact through the matching close quote, keeping
        // the quotes so the shape of the log line survives.
        if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
            let q = bytes[j];
            let mut k = j + 1;
            while k < bytes.len() && bytes[k] != q {
                k += 1;
            }
            if k < bytes.len() && k > j + 1 {
                out.push(q as char);
                out.push_str("[redacted]");
                out.push(q as char);
                i = k + 1;
                continue;
            }
            // Unterminated quote: fall through to the plain scan below.
        }
        let mut k = j;
        while k < bytes.len() {
            let b = bytes[k];
            if b.is_ascii_whitespace()
                || matches!(b, b'"' | b'\'' | b',' | b';' | b'}' | b']' | b')' | b'>')
                || !b.is_ascii()
            {
                break;
            }
            k += 1;
        }
        if k == j {
            // Separator with no value after it: undo the push, emit one
            // char, and let the rest flow through normally.
            out.truncate(out.len() - (j - i));
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        out.push_str("[redacted]");
        i = k;
    }
    out
}

/// Record an error. `notify=true` also emits `appmaka:error` so the
/// frontend pops the copyable dialog. Returns the entry id (0 when the
/// log state is unavailable, e.g. during early startup).
pub fn record<R: Runtime>(
    app: &AppHandle<R>,
    kind: &str,
    message: &str,
    technical: &str,
    notify: bool,
) -> u64 {
    let entry = {
        let Some(state) = app.try_state::<Mutex<ErrorLog>>() else {
            return 0;
        };
        let mut log = match state.lock() {
            Ok(l) => l,
            Err(_) => return 0,
        };
        log.next_id += 1;
        let e = ErrorEntry {
            id: log.next_id,
            unix_secs: unix_secs(),
            kind: kind.to_string(),
            message: message.to_string(),
            technical: sanitize(technical),
        };
        log.entries.push_back(e.clone());
        while log.entries.len() > CAP {
            log.entries.pop_front();
        }
        persist(&log);
        e
    };
    if notify {
        let _ = app.emit("appmaka:error", &entry);
    }
    entry.id
}

fn recent_locked(log: &Mutex<ErrorLog>) -> Vec<ErrorEntry> {
    match log.lock() {
        Ok(l) => l.entries.iter().rev().cloned().collect(),
        Err(_) => Vec::new(),
    }
}

fn entry_text(e: &ErrorEntry) -> String {
    format!(
        "[{}] {}\n{}\nTechnical: {}",
        format_unix_utc(e.unix_secs),
        e.kind,
        e.message,
        e.technical
    )
}

/// "windows (x86_64)" — OS plus architecture, the two facts a bug
/// report from another machine most often turns out to hinge on.
fn platform_label() -> String {
    format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH)
}

/// Full diagnostics text: version + OS + every recent error, newest first.
pub fn format_diagnostics(entries: &[ErrorEntry]) -> String {
    let mut out = format!(
        "AppMaka {} on {} — error report\n",
        env!("CARGO_PKG_VERSION"),
        platform_label()
    );
    if entries.is_empty() {
        out.push_str("No errors recorded.\n");
        return out;
    }
    for e in entries {
        out.push_str(&entry_text(e));
        out.push('\n');
    }
    out
}

fn write_clipboard(app: &AppHandle, text: &str) -> Result<(), String> {
    app.clipboard()
        .write_text(text)
        .map_err(|e| format!("Could not copy to the clipboard: {e}"))
}

/// JS: `invoke("get_recent_errors")` — newest first.
#[tauri::command]
pub fn get_recent_errors(app: AppHandle) -> Vec<ErrorEntry> {
    app.try_state::<Mutex<ErrorLog>>()
        .map(|s| recent_locked(&s))
        .unwrap_or_default()
}

/// JS: `invoke("copy_error_details", { id })` — copies one entry with
/// version + OS, the exact text the dialog's Copy button uses.
#[tauri::command]
pub fn copy_error_details(app: AppHandle, id: u64) -> Result<(), String> {
    let entry = app
        .try_state::<Mutex<ErrorLog>>()
        .and_then(|s| recent_locked(&s).into_iter().find(|e| e.id == id))
        .ok_or_else(|| "That error is no longer in the log.".to_string())?;
    let text = format!(
        "AppMaka {} on {}\n\n{}",
        env!("CARGO_PKG_VERSION"),
        platform_label(),
        entry_text(&entry)
    );
    write_clipboard(&app, &text)
}

/// JS: `invoke("record_frontend_error", { kind, message, technical })` —
/// records a frontend-initiated failure (a tile click whose invoke
/// returned an error) in the same sanitized ring the settings error
/// list shows. Recorded only, never notified: the frontend already
/// shows its own inline error for these, and a second dialog would
/// double the noise.
#[tauri::command]
pub fn record_frontend_error(app: AppHandle, kind: String, message: String, technical: String) {
    record(&app, &kind, &message, &technical, false);
}

/// JS: `invoke("copy_diagnostics")` — copies everything for pasting into
/// chat. Returns the text so the UI can confirm what was copied.
#[tauri::command]
pub fn copy_diagnostics(app: AppHandle) -> Result<String, String> {
    let entries = get_recent_errors(app.clone());
    let text = format_diagnostics(&entries);
    write_clipboard(&app, &text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::{
        format_diagnostics, format_unix_utc, parse_entries, platform_label, sanitize,
        serialize_entries, ErrorEntry, CAP,
    };
    use std::collections::VecDeque;

    #[test]
    fn utc_formatting_known_value() {
        // 2026-10-07 02:06:04 UTC
        assert_eq!(format_unix_utc(1791338764), "2026-10-07 02:06:04 UTC");
    }

    #[test]
    fn utc_formatting_epoch() {
        assert_eq!(format_unix_utc(0), "1970-01-01 00:00:00 UTC");
    }

    #[test]
    fn sanitize_redacts_assignments() {
        let s = sanitize("login failed: token=abc123 for user");
        assert!(s.contains("token=[redacted]"), "{s}");
        assert!(!s.contains("abc123"), "{s}");
    }

    #[test]
    fn sanitize_redacts_colon_and_case() {
        let s = sanitize("Password: s3cr3t! then Api-Key=\"k-9\" end");
        assert!(s.contains("Password: [redacted]"), "{s}");
        assert!(s.contains("Api-Key=\"[redacted]\""), "{s}");
        assert!(!s.contains("s3cr3t"), "{s}");
    }

    #[test]
    fn sanitize_leaves_normal_text_alone() {
        let s = sanitize("could not reach update server: 404 for https://x/latest.json");
        assert_eq!(s, "could not reach update server: 404 for https://x/latest.json");
    }

    #[test]
    fn sanitize_bare_key_without_value_untouched() {
        // "token" with no separator/value must not eat surrounding text.
        let s = sanitize("invalid token format");
        assert_eq!(s, "invalid token format");
    }

    fn entry(id: u64, technical: &str) -> ErrorEntry {
        ErrorEntry {
            id,
            unix_secs: 1791338764,
            kind: "window-open".to_string(),
            message: "Could not open the window.".to_string(),
            technical: sanitize(technical),
        }
    }

    #[test]
    fn diagnostics_contain_version_os_and_no_secrets() {
        let entries = vec![entry(1, "open failed; session_token=hunter2 at C:\\x")];
        let text = format_diagnostics(&entries);
        assert!(text.contains("AppMaka"), "{text}");
        assert!(text.contains(std::env::consts::OS), "{text}");
        assert!(text.contains("session_token=[redacted]"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("2026-10-07 02:06:04 UTC"), "{text}");
    }

    #[test]
    fn diagnostics_empty_state() {
        let text = format_diagnostics(&[]);
        assert!(text.contains("No errors recorded"), "{text}");
    }

    #[test]
    fn diagnostics_header_names_platform() {
        let text = format_diagnostics(&[]);
        assert!(text.contains(&platform_label()), "{text}");
        assert!(text.contains(std::env::consts::ARCH), "{text}");
    }

    #[test]
    fn persistence_roundtrip_preserves_entries() {
        let entries: VecDeque<ErrorEntry> = vec![entry(1, "first"), entry(2, "second; x=1")]
            .into_iter()
            .collect();
        let back = parse_entries(&serialize_entries(&entries));
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].id, 1);
        assert_eq!(back[1].id, 2);
        assert_eq!(back[1].technical, entries[1].technical);
        assert_eq!(back[0].kind, "window-open");
    }

    #[test]
    fn parse_entries_rejects_garbage() {
        assert!(parse_entries("not json").is_empty());
        assert!(parse_entries("").is_empty());
        assert!(parse_entries("{}").is_empty());
    }

    #[test]
    fn parse_entries_caps_to_newest() {
        let many: VecDeque<ErrorEntry> =
            (1..=(CAP as u64 + 5)).map(|i| entry(i, "x")).collect();
        let back = parse_entries(&serialize_entries(&many));
        assert_eq!(back.len(), CAP);
        assert_eq!(back.front().map(|e| e.id), Some(6));
        assert_eq!(back.back().map(|e| e.id), Some(CAP as u64 + 5));
    }

    #[test]
    fn parse_entries_resanitizes_technical() {
        // A hand-edited errors.json must not smuggle secrets back into
        // copyable diagnostics.
        let raw = r#"[{"id":1,"unix_secs":1,"kind":"k","message":"m","technical":"token=abc123"}]"#;
        let back = parse_entries(raw);
        assert_eq!(back.len(), 1);
        assert!(back[0].technical.contains("token=[redacted]"), "{}", back[0].technical);
        assert!(!back[0].technical.contains("abc123"), "{}", back[0].technical);
    }
}
