//! Windows-only native tweaks for popup windows (v0.9.11).
//!
//! 1. "Add to applications…" appended to the popup's system menu
//!    (right-click the title bar), via GetSystemMenu/AppendMenuW.
//! 2. Title-bar tint via the universal chain in `tint.rs` (v0.11.0):
//!    theme-color meta → live-DOM probe → default, applied with
//!    DwmSetWindowAttribute(DWMWA_CAPTION_COLOR) on Windows 11+.
//!
//! Everything here is best-effort and infallible by contract (AGENTS.md):
//! any failure leaves the popup as a plain native window.
//!
//! ## Re-entrancy analysis (required by the v0.9.6 post-mortem rules,
//! docs/RELEASE.md §8)
//!
//! Threading model (v0.18.14): the subclass proc runs on the thread that
//! pumps the popup HWND's messages — the main thread, because wry creates
//! the HWND there. Bookkeeping is therefore a `thread_local!` map, not a
//! process-wide Mutex: the old design let a worker thread install a
//! subclass (SetWindowLongPtrW from the spawned build thread) while the
//! proc read the map from the pump thread, and let unhook remove the map
//! entry before restoring the proc, so DestroyWindow re-entered a
//! subclass whose entry was already gone. INVARIANT: every map access
//! happens on the main thread. `setup_popup_chrome` and
//! `unhook_popup_chrome_on_main_thread` both dispatch there first
//! (`run_on_main_thread`); the proc itself runs there. Any future caller
//! must go through those two functions, never touch the map directly.
//! If the proc ever ran on another thread, its map read would find
//! nothing and it would fall back to DefWindowProcW — degraded chrome,
//! never a crash.
//!
//! Win32 calls, and what each delivers synchronously:
//! - `GetSystemMenu` / `AppendMenuW` (setup, main thread): no messages
//!   delivered; the subclass isn't installed until SetWindowLongPtrW
//!   returns, so nothing can re-enter the proc. The map borrow is
//!   released before either call (two-phase: the already-installed check
//!   borrows, drops, then the Win32 calls run).
//! - `SetWindowLongPtrW` installing the subclass (setup, main thread):
//!   installs the proc; it does not deliver messages synchronously.
//!   The map borrow is released before the call; the store happens in a
//!   fresh short borrow afterwards, and a failed store restores the old
//!   proc immediately so a subclass we can't unhook never survives.
//! - `SetWindowLongPtrW` restoring the old proc (unhook + WM_NCDESTROY,
//!   main thread): restores the proc; it does not deliver messages
//!   synchronously. In unhook the restore happens with NO borrow held
//!   (read first, drop, restore, then a fresh borrow removes the entry).
//!   In the proc's WM_NCDESTROY arm the same order holds: restore, then
//!   remove, then forward the message itself to the restored proc.
//! - `CallWindowProcW` / `DefWindowProcW` (proc, main thread): forwards
//!   the message to the original proc, which may do anything a window
//!   proc does — including re-entering this proc. No borrow is held
//!   across it: all decisions (old_proc, fire, unsubclass) were copied
//!   out under a short try_borrow that is dropped first.
//! - `std::thread::spawn` from the proc (WM_SYSCOMMAND add flow):
//!   spawning never delivers messages; the real work (HTTP, window
//!   creation) runs on the detached worker, never the proc thread.
//! - The whole proc body is wrapped in `catch_unwind`: a panic across
//!   the `extern "system"` boundary would abort the process instantly
//!   (v0.9.6). On panic we fall back to `DefWindowProcW`.
//!
//! `DwmSetWindowAttribute` runs on detached tint threads inside `tint.rs`,
//! never on a proc thread; on pre-Windows-11 it fails and the bar stays
//! as-is, silently.

use std::cell::RefCell;
use std::collections::HashMap;

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

thread_local! {
    // Main-thread-only bookkeeping (see the re-entrancy analysis above):
    // the proc, the install, and the unhook all run on the thread that
    // pumps the popup HWND, and this map must never be touched from
    // anywhere else. Go through `setup_popup_chrome` /
    // `unhook_popup_chrome_on_main_thread`, never this directly.
    static SUBCLASSES: RefCell<HashMap<isize, SubclassEntry>> = RefCell::new(HashMap::new());
}

/// Install the system-menu item and subclass the popup's window proc.
/// The whole install is dispatched to the main thread (see the
/// re-entrancy analysis above): the HWND's messages are pumped there,
/// so the subclass proc and the SetWindowLongPtrW that installs it must
/// agree on the thread. `run_on_main_thread` executes inline when
/// already on the main thread (the Windows sync NewWindowRequested
/// path) and posts + blocks from a worker thread (the spawned path).
/// Only GetSystemMenu / AppendMenuW / SetWindowLongPtrW run there — no
/// window is built, so the wry#583 build-on-main-thread deadlock does
/// not apply. Never panics; any failure is silent and leaves the popup
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
    let app = app.clone();
    let label = label.to_string();
    let url = url.clone();
    // Tint-thread clones, taken before the dispatch moves the originals.
    let tint_app = app.clone();
    let tint_label = label.clone();
    let tint_url = url.clone();
    let _ = app.clone().run_on_main_thread(move || {
        // Already installed (double open of the same HWND): never
        // subclass twice — the second install would orphan the first
        // proc. Borrow released before every Win32 call below.
        let already = SUBCLASSES.with(|m| match m.try_borrow() {
            Ok(map) => map.contains_key(&hwnd_raw),
            Err(_) => false,
        });
        if already {
            return;
        }
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
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
            let old =
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, subclass_proc as *const () as isize);
            if old == 0 {
                return;
            }
            match SUBCLASSES.with(|m| {
                m.try_borrow_mut().map(|mut map| {
                    map.insert(
                        hwnd_raw,
                        SubclassEntry {
                            old_proc: old,
                            app: app.clone(),
                            label: label.clone(),
                        },
                    );
                })
            }) {
                Ok(()) => {}
                Err(_) => {
                    // Couldn't record the original proc: restore immediately so
                    // we never orphan a subclass we can't unhook.
                    SetWindowLongPtrW(hwnd, GWLP_WNDPROC, old);
                }
            }
        }
    });
    // Initial title-bar tint on its own thread (HTTP fetch, never the
    // build thread's critical path).
    std::thread::Builder::new()
        .name(format!("appmaka-popup-tint-{tint_label}"))
        .spawn(move || tint_popup_caption(&tint_app, &tint_label, &tint_url))
        .ok();
}

/// Re-tint after a navigation. Thin wrapper over the universal tint chain
/// (v0.11.0): theme-color meta → live-DOM probe → default, applied via
/// DWM. Called on a detached thread from the `on_navigation` hook (which
/// must never block the navigation decision).
pub fn tint_popup_caption(app: &AppHandle, label: &str, url: &url::Url) {
    crate::tint::request_retint(app, label, url, crate::tint::TintTarget::NativeCaption);
}

/// Unhook the subclass proc ahead of a window destroy. MUST be called
/// on the main thread — the bookkeeping map is thread-local there (see
/// the invariant in the file header). The only caller is
/// `popup_bar::destroy_webview_then_window`, which dispatches to the
/// main thread first. Restore the original proc FIRST, then drop the
/// bookkeeping entry: DestroyWindow must never re-enter a subclass
/// whose entry is already gone (two-phase rule, file header). The
/// WM_NCDESTROY path in the proc itself stays the backstop when the
/// window is already gone.
pub fn unhook_popup_chrome_on_main_thread(window: &tauri::WebviewWindow) {
    let hwnd = match window.hwnd() {
        Ok(h) => h,
        Err(_) => return,
    };
    let hwnd_raw = hwnd.0 as isize;
    // Phase 1 — decide under a short, non-blocking borrow. Released before
    // the Win32 call below. On contention we leave the subclass in place:
    // without the entry we don't know the original proc, and the proc's
    // own WM_NCDESTROY arm restores it.
    let old_proc = SUBCLASSES.with(|m| match m.try_borrow() {
        Ok(map) => map.get(&hwnd_raw).map(|e| e.old_proc),
        Err(_) => None,
    });
    let Some(old_proc) = old_proc else {
        return;
    };
    // Phase 2 — restore first, with no borrow held.
    unsafe {
        if old_proc != 0 {
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, old_proc);
        }
    }
    // Phase 3 — restore done, now drop the entry (no Win32 call after).
    SUBCLASSES.with(|m| {
        if let Ok(mut map) = m.try_borrow_mut() {
            map.remove(&hwnd_raw);
        }
    });
}

/// The subclass proc. See the re-entrancy analysis at the top of this
/// file: `catch_unwind` outside everything, `try_borrow` only, no borrow
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
    // Phase 1 — decide under a short, non-blocking borrow. Released before
    // every Win32 call below.
    let (old_proc, fire_add, unsubclass, unsub_label) = SUBCLASSES.with(|m| match m.try_borrow() {
        Ok(map) => match map.get(&hwnd_raw) {
            Some(e) => {
                let fire = msg == WM_SYSCOMMAND && (wparam.0 as u32) == IDM_ADD_TO_APPLICATIONS;
                let unsub = msg == WM_NCDESTROY;
                (
                    e.old_proc,
                    fire.then(|| (e.app.clone(), e.label.clone())),
                    unsub,
                    unsub.then(|| (e.app.clone(), e.label.clone())),
                )
            }
            None => (0, None, false, None),
        },
        // Borrow contended: skip our handling entirely, just forward.
        Err(_) => (0, None, false, None),
    });

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
            // Restore the original proc first (no borrow held), then drop
            // the bookkeeping, then forward the message itself.
            if old_proc != 0 {
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, old_proc);
            }
            SUBCLASSES.with(|m| {
                if let Ok(mut map) = m.try_borrow_mut() {
                    map.remove(&hwnd_raw);
                }
            });
            // Close-lifecycle trace: the last close step for a popup's
            // native chrome. If this never logs for a closed popup, the
            // subclass (and its window) never tore down. File I/O goes
            // on a detached thread — never file work on the proc.
            if let Some((app, label)) = unsub_label {
                let _ = std::thread::Builder::new()
                    .name(format!("appmaka-popup-lifecycle-{label}"))
                    .spawn(move || {
                        crate::windows::log_popup_lifecycle(
                            &app,
                            &label,
                            "wm-ncdestroy-subclass-removed",
                        );
                    });
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
    // The range invariant is enforced at compile time by the const block
    // next to the id; this test documents the intent for readers.
    #[test]
    fn menu_id_documented() {
        assert_eq!(super::IDM_ADD_TO_APPLICATIONS, 0x0A11);
    }
}
