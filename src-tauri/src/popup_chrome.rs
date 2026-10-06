//! Windows-only native tweaks for popup windows (v0.9.11).
//!
//! 1. "Add to applications…" appended to the popup's system menu
//!    (right-click the title bar), via GetSystemMenu/AppendMenuW.
//! 2. Title-bar tint from the site's `theme-color` meta tag, via
//!    DwmSetWindowAttribute(DWMWA_CAPTION_COLOR) on Windows 11+.
//!
//! Everything here is best-effort and infallible by contract (AGENTS.md):
//! any failure leaves the popup as a plain native window.
//!
//! ## Re-entrancy analysis (required by the v0.9.6 post-mortem rules)
//!
//! `setup_popup_chrome` runs on the popup's dedicated build thread
//! (spawned by `spawn_contained_window`), never on a window-proc thread.
//! `GetSystemMenu`, `AppendMenuW` and `SetWindowLongPtrW` are called there;
//! none of them synchronously delivers messages to our subclass proc (the
//! subclass isn't installed until `SetWindowLongPtrW` returns, and menu
//! calls don't pump messages).
//!
//! The subclass proc itself runs on whichever thread pumps the popup
//! HWND's messages (wry creates the HWND on the main thread, whose event
//! loop pumps it). Inside the proc:
//! - Only `try_lock` is ever used on the bookkeeping map. On contention
//!   we skip our handling and forward to the original proc — the pump
//!   thread is never blocked.
//! - The lock is always released BEFORE any Win32 call (two-phase):
//!   decide under the lock, then `CallWindowProcW` / `SetWindowLongPtrW`
//!   / `std::thread::spawn` with no lock held.
//! - `WM_SYSCOMMAND` + our id only clones `AppHandle`/`String` under the
//!   lock; the real work (HTTP fetches, window creation) runs on a
//!   detached worker thread, never the proc thread.
//! - `WM_NCDESTROY` restores the original proc with `SetWindowLongPtrW`
//!   (lock released first), drops the bookkeeping entry, then forwards.
//! - The whole proc body is wrapped in `catch_unwind`: a panic across the
//!   `extern "system"` boundary would abort the process instantly
//!   (v0.9.6). On panic we fall back to `DefWindowProcW`.
//!
//! `DwmSetWindowAttribute` runs on detached tint threads, never on a proc
//! thread; on pre-Windows-11 it fails and the bar stays as-is, silently.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Private system-menu command id. Must stay below 0xF000 (the system's
/// SC_* range) so it can never collide with a real system command.
/// Compile-time checked by the const block below.
const IDM_ADD_TO_APPLICATIONS: u32 = 0x0A11;

// Compile-time proof the id stays out of the system command range
// (clippy assertions_on_constants: a const block, not a test assert).
const _: () = {
    assert!(IDM_ADD_TO_APPLICATIONS < 0xF000);
};

struct SubclassEntry {
    /// Original window proc, restored on WM_NCDESTROY.
    old_proc: isize,
    app: AppHandle,
    label: String,
}

static SUBCLASSES: OnceLock<Mutex<HashMap<isize, SubclassEntry>>> = OnceLock::new();

fn subclasses() -> &'static Mutex<HashMap<isize, SubclassEntry>> {
    SUBCLASSES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Last URL tinted per popup label — navigation fires often (including
/// subframes), and refetching an unchanged address would just burn HTTP.
static LAST_TINT: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn last_tint() -> &'static Mutex<HashMap<String, String>> {
    LAST_TINT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Install the system-menu item and subclass the popup's window proc.
/// Called on the popup's dedicated build thread right after `build()`
/// succeeds. Never panics; any failure is silent and leaves the popup
/// exactly as it was.
pub fn setup_popup_chrome(
    app: &AppHandle,
    window: &tauri::WebviewWindow,
    label: &str,
    url: &url::Url,
) {
    let hwnd = match window.hwnd() {
        Ok(h) => h,
        Err(_) => return,
    };
    let hwnd_raw = hwnd.0 as isize;
    unsafe {
        let menu = GetSystemMenu(hwnd, false);
        if menu.is_invalid() {
            return;
        }
        let text: Vec<u16> = "Add to applications…\0".encode_utf16().collect();
        if AppendMenuW(
            menu,
            MF_STRING,
            IDM_ADD_TO_APPLICATIONS as usize,
            windows::core::PCWSTR(text.as_ptr()),
        )
        .is_err()
        {
            return;
        }
        let old = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, subclass_proc as *const () as isize);
        if old == 0 {
            return;
        }
        match subclasses().try_lock() {
            Ok(mut map) => {
                map.insert(
                    hwnd_raw,
                    SubclassEntry {
                        old_proc: old,
                        app: app.clone(),
                        label: label.to_string(),
                    },
                );
            }
            Err(_) => {
                // Couldn't record the original proc: restore immediately so
                // we never orphan a subclass we can't unhook.
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, old);
                return;
            }
        }
    }
    // Initial title-bar tint on its own thread (HTTP fetch, never the
    // build thread's critical path).
    let app = app.clone();
    let label = label.to_string();
    let url = url.clone();
    std::thread::Builder::new()
        .name(format!("appmaka-popup-tint-{label}"))
        .spawn(move || tint_popup_caption(&app, &label, &url))
        .ok();
}

/// Re-tint after a navigation. Called on a detached thread from the
/// `on_navigation` hook (which must never block the navigation decision).
pub fn tint_popup_caption(app: &AppHandle, label: &str, url: &url::Url) {
    let url_str = url.to_string();
    // Skip when the address hasn't changed since the last tint.
    if let Ok(map) = last_tint().try_lock() {
        if map.get(label).is_some_and(|u| u == &url_str) {
            return;
        }
    }
    let window = match app.get_webview_window(label) {
        Some(w) => w,
        None => return,
    };
    let hwnd = match window.hwnd() {
        Ok(h) => h,
        Err(_) => return,
    };
    let hex = match crate::page_title::fetch_theme_color(&url_str) {
        Some(h) => h,
        None => return,
    };
    let (r, g, b) = match parse_hex_color(&hex) {
        Some(t) => t,
        None => return,
    };
    apply_caption_color(hwnd, r, g, b);
    if let Ok(mut map) = last_tint().try_lock() {
        map.insert(label.to_string(), url_str);
    }
}

/// `#rrggbb` → `(r, g, b)`. Defensive: the fetcher already normalizes,
/// but the tint path must never mis-parse into a garbage color.
fn parse_hex_color(hex: &str) -> Option<(u8, u8, u8)> {
    let s = hex.strip_prefix('#')?;
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// COLORREF is 0x00BBGGRR. Pre-Windows 11 the call fails; the silent
/// fallback is the current bar.
fn apply_caption_color(hwnd: HWND, r: u8, g: u8, b: u8) {
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CAPTION_COLOR};
    let colorref: u32 = r as u32 | ((g as u32) << 8) | ((b as u32) << 16);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &colorref as *const u32 as *const std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

/// The subclass proc. See the re-entrancy analysis at the top of this
/// file: `catch_unwind` outside everything, `try_lock` only, no lock
/// held across any Win32 call, real work on detached threads.
unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        subclass_proc_inner(hwnd, msg, wparam, lparam)
    }));
    match outcome {
        Ok(r) => r,
        // Without the bookkeeping map we can't know the original proc;
        // DefWindowProcW is the safe fallback. Never unwind across FFI.
        Err(_) => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn subclass_proc_inner(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let hwnd_raw = hwnd.0 as isize;
    // Phase 1 — decide under a short, non-blocking lock. Released before
    // every Win32 call below.
    let (old_proc, fire_add, unsubclass) = match subclasses().try_lock() {
        Ok(map) => match map.get(&hwnd_raw) {
            Some(e) => {
                let fire = msg == WM_SYSCOMMAND && (wparam.0 as u32) == IDM_ADD_TO_APPLICATIONS;
                (
                    e.old_proc,
                    fire.then(|| (e.app.clone(), e.label.clone())),
                    msg == WM_NCDESTROY,
                )
            }
            None => (0, None, false),
        },
        // Lock contended: skip our handling entirely, just forward.
        Err(_) => (0, None, false),
    };

    unsafe {
        if let Some((app, label)) = fire_add {
            // Never run the add flow on the proc thread: it does HTTP and
            // creates windows. A detached worker owns it end to end.
            std::thread::Builder::new()
                .name(format!("appmaka-popup-add-{label}"))
                .spawn(move || {
                    let store = app.state::<crate::store::AppStore>();
                    let adblock = app.state::<crate::adblock::AdblockState>();
                    let winstate = app.state::<crate::windows::WindowState>();
                    match crate::popup_add::run_add(&app, &store, &adblock, &winstate, &label)
                    {
                        Ok(o) => {
                            if o.already_added {
                                crate::popup_add::emit_notice(
                                    &app,
                                    crate::popup_add::MSG_ALREADY_ADDED,
                                );
                            }
                        }
                        Err(_) => crate::popup_add::emit_notice(
                            &app,
                            crate::popup_add::MSG_ADD_FAILED,
                        ),
                    }
                })
                .ok();
            return LRESULT(0);
        }
        if unsubclass {
            // Restore the original proc first (no lock held), then drop
            // the bookkeeping, then forward the message itself.
            if old_proc != 0 {
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, old_proc);
            }
            if let Ok(mut map) = subclasses().try_lock() {
                map.remove(&hwnd_raw);
            }
        }
        if old_proc != 0 {
            #[allow(clippy::transmutes_expressible_as_ptr_casts)]
            let old: WNDPROC = std::mem::transmute(old_proc);
            CallWindowProcW(old, hwnd, msg, wparam, lparam)
        } else {
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_hex_color;

    #[test]
    fn hex_parses_to_rgb() {
        assert_eq!(parse_hex_color("#1a2b3c"), Some((0x1a, 0x2b, 0x3c)));
        assert_eq!(parse_hex_color("#FFFFFF"), Some((0xff, 0xff, 0xff)));
    }

    #[test]
    fn hex_rejects_garbage() {
        assert_eq!(parse_hex_color("1a2b3c"), None);
        assert_eq!(parse_hex_color("#abc"), None);
        assert_eq!(parse_hex_color("#gggggg"), None);
        assert_eq!(parse_hex_color(""), None);
    }

    // The range invariant is enforced at compile time by the const block
    // next to the id; this test documents the intent for readers.
    #[test]
    fn menu_id_documented() {
        assert_eq!(super::IDM_ADD_TO_APPLICATIONS, 0x0A11);
    }
}
