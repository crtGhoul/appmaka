//! Popup bar (v0.18.7): one "Save as app" button docked under a popup window.
//!
//! Popups are raw site windows with no AppMaka chrome and no IPC
//! (`withGlobalTauri` is false crate-wide, see `spawn_contained_window`),
//! so a visible save button cannot live in the page itself. It lives in
//! our own trusted window instead — the same reason tabbed windows get a
//! strip window. The bar sits directly under the popup, follows it on
//! move/resize, hides when the popup minimizes, and dies with it.
//!
//! OAuth modals get no bar: they are transient sign-in windows, and
//! `popup_add::add_label_allowed` rejects them anyway. Drawing a button
//! that can never work would break the anti-slop rule, so the bar is
//! attached for `popup` windows only.
//!
//! Everything here is best-effort and infallible by contract: any failure
//! leaves the popup exactly as it was, without a bar.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::windows::WindowPlacement;
use tauri::{AppHandle, Manager, Position, Size, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

/// Labels of popups with a teardown already in flight. A second close
/// request for the same label is a no-op — destroying a window twice is
/// how the close path used to re-enter itself.
static CLOSING_POPUPS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn closing_set() -> &'static Mutex<HashSet<String>> {
    CLOSING_POPUPS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Claim a label for teardown. False means a close is already in flight
/// (dedup, silent) or the set lock stayed contended after one retry
/// (logged loudly — the caller must not silently swallow a stuck close).
fn begin_closing(label: &str) -> bool {
    // try_lock only: a window-proc thread may hold this set while a
    // re-entrant close arrives, and blocking here would deadlock it.
    match closing_set().try_lock() {
        Ok(mut set) => set.insert(label.to_string()),
        Err(_) => {
            std::thread::sleep(Duration::from_millis(2));
            match closing_set().try_lock() {
                Ok(mut set) => set.insert(label.to_string()),
                Err(_) => {
                    eprintln!("[appmaka] popup close lock contended for {label}; close abandoned, retry by closing again");
                    false
                }
            }
        }
    }
}

fn finish_closing(label: &str) {
    match closing_set().try_lock() {
        Ok(mut set) => {
            set.remove(label);
        }
        Err(_) => {
            std::thread::sleep(Duration::from_millis(2));
            if let Ok(mut set) = closing_set().try_lock() {
                set.remove(label);
            } else {
                // The entry leaks: the next close of this label dedups
                // away. Loud, because a stuck label is a stuck window.
                eprintln!("[appmaka] popup close lock contended finishing {label}; label stays marked closing");
            }
        }
    }
}

/// Destroy a window's webview controller before its native window.
/// Closing a sign-in popup the other way round — DestroyWindow while
/// WebView2's controller is still live — re-enters the window proc and
/// freezes the whole app (the close event pumps while the controller
/// waits on its own UI thread). Hide first so nothing paints mid-teardown,
/// unhook the Win32 subclass so DestroyWindow never re-enters it, then
/// close the controller, then the window. No lock is held across any of it.
///
/// The unhook + controller close run on the main thread, and this
/// function runs on the teardown worker thread (see `close_popup_async`),
/// so the main-thread work is posted via `run_on_main_thread` and the
/// worker waits for it — bounded at 300ms. `with_webview` posts to the
/// main thread and returns immediately, so destroying the window right
/// after it used to race the controller close: that race is exactly
/// what this wait closes. A controller that hasn't closed in 300ms is
/// torn down with its window.
pub fn destroy_webview_then_window(window: &WebviewWindow) {
    let _ = window.hide();
    // popup_chrome is Windows-only (main.rs gates the module); elsewhere
    // there is no subclass to unhook.
    #[cfg(windows)]
    {
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        // app_handle() borrows the window — clone it: the closure below
        // must be 'static, so nothing borrowed from `window` may escape.
        let app = window.app_handle().clone();
        let label = window.label().to_string();
        let _ = app.clone().run_on_main_thread(move || {
            if let Some(win) = app.get_webview_window(&label) {
                crate::popup_chrome::unhook_popup_chrome_on_main_thread(&win);
                let _ = win.with_webview(|wv| unsafe {
                    let controller = wv.controller();
                    let _ = controller.SetIsVisible(false);
                    let _ = controller.Close();
                });
            }
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(Duration::from_millis(300));
    }

    let _ = window.destroy();
}

/// Tear a popup down off the UI thread. The close event that triggers
/// this runs inside the window proc's message dispatch — destroying the
/// webview synchronously there is the freeze. The bar goes first (it is
/// owned by the popup), then the popup itself.
pub fn close_popup_async(app: &AppHandle, raw_label: &str) {
    let popup_label = raw_label
        .strip_prefix("popupbar-")
        .unwrap_or(raw_label)
        .to_string();

    if !begin_closing(&popup_label) {
        return;
    }

    let app_handle = app.clone();
    let _ = std::thread::Builder::new()
        .name(format!("appmaka-popup-close-{popup_label}"))
        .spawn(move || {
            let bar_label = popup_bar_label(&popup_label);

            // 1. Destroy the companion bar webview + window first.
            if let Some(bar) = app_handle.get_webview_window(&bar_label) {
                destroy_webview_then_window(&bar);
            }

            // 2. Destroy the popup's webview controller before its HWND.
            if let Some(popup) = app_handle.get_webview_window(&popup_label) {
                destroy_webview_then_window(&popup);
            }

            finish_closing(&popup_label);
        });
}

/// Bar height in logical pixels.
pub const POPUP_BAR_H: f64 = 36.0;

/// Window label for a popup's bar.
pub fn popup_bar_label(popup_label: &str) -> String {
    format!("popupbar-{popup_label}")
}

/// Pure geometry: the bar rect (logical x, y, w, h) for a popup on a
/// monitor. Two rects, not eight floats — the arg order of eight f64s
/// is a silent-swap footgun at the call sites.
///
/// Below the popup when it fits on the monitor, else above it (the tab
/// strip docks the same way). The bar is always the popup's full width,
/// clamped inside the monitor.
pub fn bar_rect(popup: WindowPlacement, monitor: WindowPlacement) -> (f64, f64, f64, f64) {
    let w = popup.width.min(monitor.width).max(0.0);
    let x = popup
        .x
        .clamp(monitor.x, (monitor.x + monitor.width - w).max(monitor.x));
    let below_y = popup.y + popup.height;
    let y = if below_y + POPUP_BAR_H <= monitor.y + monitor.height {
        below_y
    } else {
        (popup.y - POPUP_BAR_H).max(monitor.y)
    };
    (x, y, w, POPUP_BAR_H)
}

/// Reposition the bar under its popup (or above it when there is no room
/// below). Reads the popup's live geometry; silent when either window is
/// gone — the Destroyed handler owns that case.
fn reposition_bar(app: &AppHandle, popup_label: &str) {
    let Some(popup) = app.get_webview_window(popup_label) else {
        return;
    };
    let Some(bar) = app.get_webview_window(&popup_bar_label(popup_label)) else {
        return;
    };
    let scale = popup.scale_factor().unwrap_or(1.0).max(0.1);
    let (Ok(p), Ok(s)) = (popup.outer_position(), popup.outer_size()) else {
        return;
    };
    let (mx, my, mw, mh) = popup
        .current_monitor()
        .ok()
        .flatten()
        .map(|m| {
            let pos = m.position();
            let size = m.size();
            (
                pos.x as f64 / scale,
                pos.y as f64 / scale,
                size.width as f64 / scale,
                size.height as f64 / scale,
            )
        })
        .unwrap_or((0.0, 0.0, p.x as f64 / scale + s.width as f64 / scale, 10_000.0));
    let (x, y, w, h) = bar_rect(
        WindowPlacement {
            x: p.x as f64 / scale,
            y: p.y as f64 / scale,
            width: s.width as f64 / scale,
            height: s.height as f64 / scale,
        },
        WindowPlacement {
            x: mx,
            y: my,
            width: mw,
            height: mh,
        },
    );
    let _ = bar.set_position(Position::Logical(tauri::LogicalPosition::new(x, y)));
    let _ = bar.set_size(Size::Logical(tauri::LogicalSize::new(w, h)));
}

/// Build the bar for a popup and wire it to the popup's lifetime. Call
/// once, right after the popup window is built. Never panics; a failed
/// build leaves the popup bar-less.
pub fn attach_popup_bar(app: &AppHandle, popup: &WebviewWindow, popup_label: &str) {
    let scale = popup.scale_factor().unwrap_or(1.0).max(0.1);
    let (px, py, pw, ph) = match (popup.outer_position(), popup.outer_size()) {
        (Ok(p), Ok(s)) => (
            p.x as f64 / scale,
            p.y as f64 / scale,
            s.width as f64 / scale,
            s.height as f64 / scale,
        ),
        _ => return,
    };
    let (mx, my, mw, mh) = popup
        .current_monitor()
        .ok()
        .flatten()
        .map(|m| {
            let pos = m.position();
            let size = m.size();
            (
                pos.x as f64 / scale,
                pos.y as f64 / scale,
                size.width as f64 / scale,
                size.height as f64 / scale,
            )
        })
        .unwrap_or((0.0, 0.0, px + pw, 10_000.0));
    let (x, y, w, h) = bar_rect(
        WindowPlacement {
            x: px,
            y: py,
            width: pw,
            height: ph,
        },
        WindowPlacement {
            x: mx,
            y: my,
            width: mw,
            height: mh,
        },
    );
    let mut bar_builder = WebviewWindowBuilder::new(
        app,
        popup_bar_label(popup_label),
        WebviewUrl::App("popupbar.html".into()),
    )
    .title("Save as app")
    .decorations(false)
    .skip_taskbar(true)
    .focused(false)
    .inner_size(w, h)
    .position(x, y);
    // The bar is a separate window over the popup: without the cursor
    // init script the drawn pointer parks on the page behind it while
    // the native cursor moves in the bar — the same two-cursor effect
    // as an iframe edge.
    let cursor_js = crate::windows::saved_cursor_js(app);
    if !cursor_js.is_empty() {
        bar_builder = bar_builder.initialization_script(cursor_js);
    }
    let bar = match bar_builder.build() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[appmaka] popup bar failed for {popup_label}: {e}");
            return;
        }
    };
    let _ = bar;
    // Follow the popup: re-seat on move/resize, hide while minimized,
    // close when the popup dies. The getters run inline here, the same
    // way the tab strip's own Moved handler does (tabs.rs).
    let ev_app = app.clone();
    let ev_label = popup_label.to_string();
    popup.on_window_event(move |event| match event {
        WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
            reposition_bar(&ev_app, &ev_label);
        }
        WindowEvent::Focused(focused) => {
            if *focused {
                // Focused-back focus: route focus into the popup's
                // webview, or clicks land on a dead page.
                #[cfg(windows)]
                crate::windows::focus_webview(&ev_app, &ev_label);
                if let Some(b) = ev_app.get_webview_window(&popup_bar_label(&ev_label)) {
                    let _ = b.show();
                }
            } else if ev_app
                .get_webview_window(&ev_label)
                .and_then(|w| w.is_minimized().ok())
                .unwrap_or(false)
            {
                if let Some(b) = ev_app.get_webview_window(&popup_bar_label(&ev_label)) {
                    let _ = b.hide();
                }
            }
        }
        WindowEvent::Destroyed => {
            // The popup itself is already gone; this only matters when
            // the close arrived without a CloseRequested (or the async
            // teardown raced it) — the dedup set makes a second call a
            // no-op, so this is safe to run unconditionally.
            close_popup_async(&ev_app, &ev_label);
        }
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(x: f64, y: f64, width: f64, height: f64) -> WindowPlacement {
        WindowPlacement {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn bar_label_prefixes_popup_label() {
        assert_eq!(popup_bar_label("popup-3"), "popupbar-popup-3");
    }

    #[test]
    fn bar_docks_below_when_it_fits() {
        // Popup 640x720 at (100, 100) on a 1920x1080 monitor.
        assert_eq!(
            bar_rect(
                place(100.0, 100.0, 640.0, 720.0),
                place(0.0, 0.0, 1920.0, 1080.0)
            ),
            (100.0, 820.0, 640.0, POPUP_BAR_H)
        );
    }

    #[test]
    fn bar_docks_above_when_below_would_leave_the_monitor() {
        // Popup bottom at 1060; the bar needs 36 more, the monitor ends
        // at 1080 — so it goes above the popup instead.
        assert_eq!(
            bar_rect(
                place(100.0, 340.0, 640.0, 720.0),
                place(0.0, 0.0, 1920.0, 1080.0)
            ),
            (100.0, 304.0, 640.0, POPUP_BAR_H)
        );
    }

    #[test]
    fn bar_clamps_inside_the_monitor() {
        // Popup wider than the monitor and hanging off the left edge.
        let (x, y, w, h) = bar_rect(
            place(-50.0, 100.0, 2000.0, 720.0),
            place(0.0, 0.0, 1920.0, 1080.0),
        );
        assert_eq!((x, w, h), (0.0, 1920.0, POPUP_BAR_H));
        assert_eq!(y, 820.0);
    }
}
