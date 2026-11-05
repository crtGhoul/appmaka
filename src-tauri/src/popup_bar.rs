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

use crate::windows::WindowPlacement;
use tauri::{AppHandle, Manager, Position, Size, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

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
    let bar = match WebviewWindowBuilder::new(
        app,
        popup_bar_label(popup_label),
        WebviewUrl::App("popupbar.html".into()),
    )
    .title("Save as app")
    .decorations(false)
    .skip_taskbar(true)
    .focused(false)
    .inner_size(w, h)
    .position(x, y)
    .build()
    {
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
            if let Some(b) = ev_app.get_webview_window(&popup_bar_label(&ev_label)) {
                let _ = b.close();
            }
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
