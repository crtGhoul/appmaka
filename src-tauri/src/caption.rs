//! Slim custom caption bars for page windows (v0.9.6, Windows only).
//!
//! Account windows and the search window load arbitrary external sites, so
//! they must never expose the Tauri IPC bridge (`withGlobalTauri: false`).
//! That rules out an HTML caption bar: `data-tauri-drag-region` and any
//! button both need `window.__TAURI_INTERNALS__`, which is absent by design.
//! Instead, each page window goes frameless (`decorations(false)`, keeping
//! its taskbar button, Alt+Space menu, and tao's emulated border resizing)
//! and gets a tiny native caption strip: one `WS_POPUP` HWND owned by the
//! page window, painted dark, carrying exactly one button (minimize). The
//! strip lives on a dedicated chrome thread with its own message pump.
//!
//! The same thread owns the close-gesture hook (WH_KEYBOARD_LL): on Esc
//! key-down it checks `GetAsyncKeyState(VK_LBUTTON)` — no second mouse hook
//! needed — and closes the window only when the foreground window is one of
//! ours (a page window or its caption). Esc alone, or Esc+LMB anywhere else,
//! passes through untouched. The hook installs with the first caption and
//! uninstalls with the last one: there is no session-wide hook while no
//! page window exists.
//!
//! The gesture classifier is pure logic (events in, close-or-not out) and is
//! unit-tested on every platform; only the Win32 machinery is cfg(windows).

/// One decoded close-gesture event.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscGesture {
    /// An Esc key-down arrived (auto-repeat counts: the window is gone after
    /// the first close, so repeats are harmless no-ops).
    pub esc_down: bool,
    /// The left mouse button is physically held right now.
    pub lmb_held: bool,
    /// The foreground window is an account/search window (or its caption).
    /// A minimized window can never be foreground, so it can never match.
    pub foreground_is_page: bool,
}

/// True when the gesture should close the focused page window: Esc pressed
/// while the left button is held, with a page window in the foreground.
/// Every other combination passes through untouched.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub fn esc_gesture_closes(g: EscGesture) -> bool {
    g.esc_down && g.lmb_held && g.foreground_is_page
}

// ---------------------------------------------------------------------------
// Maximize/restore + snap: pure, cross-platform, unit-tested on every
// platform (only the Win32 machinery is cfg(windows)).
// ---------------------------------------------------------------------------

/// Snap zones for the strip's own right-click snap menu (v0.9.9). The
/// native Windows 11 snap-layouts flyout is not reachable from our
/// architecture — the shell only offers it to a window answering
/// WM_NCHITTEST with HTMAXBUTTON, and our maximize button lives in an
/// owned WS_POPUP strip above the page, not in the page window itself —
/// so right-clicking the maximize button opens this minimal native menu
/// instead. (Win+Z and Win+Arrow keep working on the page directly.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub enum SnapZone {
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[cfg_attr(not(any(test, windows)), allow(dead_code))]
impl SnapZone {
    /// Menu order, matching the native flyout's most-used layouts first.
    pub fn menu_order() -> [SnapZone; 6] {
        use SnapZone::*;
        [Left, Right, TopLeft, TopRight, BottomLeft, BottomRight]
    }

    pub fn menu_label(self) -> &'static str {
        match self {
            SnapZone::Left => "Snap left",
            SnapZone::Right => "Snap right",
            SnapZone::TopLeft => "Snap top left",
            SnapZone::TopRight => "Snap top right",
            SnapZone::BottomLeft => "Snap bottom left",
            SnapZone::BottomRight => "Snap bottom right",
        }
    }
}

/// Pure: monitor work-area (left, top, right, bottom) -> window rect
/// (x, y, w, h) for a snap zone. Halves split the work area; quadrants
/// split it into four.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub fn snap_zone_rect(work: (i32, i32, i32, i32), zone: SnapZone) -> (i32, i32, i32, i32) {
    let (l, t, r, b) = work;
    let w = r - l;
    let h = b - t;
    let hw = w / 2;
    let hh = h / 2;
    match zone {
        SnapZone::Left => (l, t, hw, h),
        SnapZone::Right => (l + hw, t, w - hw, h),
        SnapZone::TopLeft => (l, t, hw, hh),
        SnapZone::TopRight => (l + hw, t, w - hw, hh),
        SnapZone::BottomLeft => (l, t + hh, hw, h - hh),
        SnapZone::BottomRight => (l + hw, t + hh, w - hw, h - hh),
    }
}

/// Pure: which system command a maximize-button click should send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub enum MaxToggle {
    Maximize,
    Restore,
}

#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub fn max_toggle(is_zoomed: bool) -> MaxToggle {
    if is_zoomed {
        MaxToggle::Restore
    } else {
        MaxToggle::Maximize
    }
}

#[cfg(test)]
mod snap_tests {
    use super::*;

    #[test]
    fn halves_split_work_area() {
        let work = (0, 0, 1920, 1040);
        assert_eq!(snap_zone_rect(work, SnapZone::Left), (0, 0, 960, 1040));
        assert_eq!(snap_zone_rect(work, SnapZone::Right), (960, 0, 960, 1040));
    }

    #[test]
    fn quadrants_tile_without_gaps() {
        let work = (0, 0, 1920, 1040);
        assert_eq!(snap_zone_rect(work, SnapZone::TopLeft), (0, 0, 960, 520));
        assert_eq!(snap_zone_rect(work, SnapZone::TopRight), (960, 0, 960, 520));
        assert_eq!(
            snap_zone_rect(work, SnapZone::BottomLeft),
            (0, 520, 960, 520)
        );
        assert_eq!(
            snap_zone_rect(work, SnapZone::BottomRight),
            (960, 520, 960, 520)
        );
    }

    #[test]
    fn odd_sizes_dont_overlap_or_gap() {
        // Integer division must not drop or double-count a pixel column.
        let work = (0, 0, 1919, 1039);
        let (lx, _, lw, _) = snap_zone_rect(work, SnapZone::Left);
        let (rx, _, rw, _) = snap_zone_rect(work, SnapZone::Right);
        assert_eq!((lx, lw, rx, rw), (0, 959, 959, 960));
        let (_, ty, _, th) = snap_zone_rect(work, SnapZone::TopLeft);
        let (_, by, _, bh) = snap_zone_rect(work, SnapZone::BottomLeft);
        assert_eq!((ty, th, by, bh), (0, 519, 519, 520));
    }

    #[test]
    fn toggle_follows_zoom_state() {
        assert_eq!(max_toggle(false), MaxToggle::Maximize);
        assert_eq!(max_toggle(true), MaxToggle::Restore);
    }

    #[test]
    fn menu_covers_six_labeled_zones() {
        let order = SnapZone::menu_order();
        assert_eq!(order.len(), 6);
        for zone in order {
            assert!(!zone.menu_label().is_empty());
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::{
        esc_gesture_closes, max_toggle, snap_zone_rect, EscGesture, MaxToggle, SnapZone,
    };
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::mpsc;
    use std::sync::OnceLock;
    use tauri::{AppHandle, Manager, WebviewWindow};
    use windows::core::{w, PWSTR};
    use windows::Win32::Foundation::*;
    use windows::Win32::Graphics::Dwm::*;
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::*;
    use windows::Win32::UI::Controls::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    use windows::Win32::UI::WindowsAndMessaging::*;

    const VK_LBUTTON_I32: i32 = 0x01;
    const VK_ESCAPE_U32: u32 = 0x1B;
    /// Caption height and minimize-button width, logical pixels.
    const BAR_H_LOGICAL: f64 = 30.0;
    const BTN_W_LOGICAL: f64 = 46.0;

    fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
        COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
    }

    /// Best-effort file log for caption-strip failures. eprintln is
    /// invisible on the Windows GUI build, so strip errors go to
    /// <app_data>/caption-errors.log (rotated past ~256 KiB) instead of
    /// relying on stderr.
    fn caption_log(app: &AppHandle, msg: &str) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Ok(dir) = app.path().app_data_dir() {
            let path = dir.join("caption-errors.log");
            if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 262_144 {
                let _ = std::fs::remove_file(&path);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                use std::io::Write as _;
                let _ = writeln!(f, "[{ts}] {msg}");
            }
        }
    }

    enum ChromeCmd {
        AddCaption {
            owner: isize,
            label: String,
            scale: f64,
            /// Window title at creation ("App — Account"), drawn in the
            /// strip so a plain page strip is a real title bar, not an
            /// empty black bar.
            title: String,
        },
        RemoveCaption {
            label: String,
        },
        Reposition {
            label: String,
        },
        ClosePage {
            label: String,
        },
        // v0.10.0: tabbed windows. The strip paints tabs from a snapshot;
        // clicks/keys arrive as commands and are fanned out to tabs.rs on
        // worker threads (window building never happens in the proc).
        AddTabbedCaption {
            owner: isize,
            label: String,
            scale: f64,
            tabs: TabStripData,
        },
        UpdateTabbedTabs {
            label: String,
            tabs: TabStripData,
        },
        SetTabTint {
            label: String,
            tint: Option<(u8, u8, u8)>,
        },
        /// v0.11.0: universal blending — tint a plain page window's strip
        /// with its resolved site color (None clears back to dark).
        SetPageTint {
            label: String,
            tint: Option<(u8, u8, u8)>,
        },
        TabKey {
            label: String,
            action: crate::tabs::TabKeyAction,
        },
    }

    /// Tab names + active index + theme tint snapshot for one tabbed strip
    /// (v0.10.0). Built by tabs.rs; the strip only ever paints from the
    /// latest snapshot — no store lookups on the chrome thread.
    #[derive(Debug, Clone)]
    pub struct TabStripData {
        pub tabs: Vec<String>,
        pub active: usize,
        pub tint: Option<(u8, u8, u8)>,
    }

    struct Caption {
        hwnd: HWND,
        owner: HWND,
        scale: f64,
        hover_min: bool,
        pressed_min: bool,
        hover_max: bool,
        pressed_max: bool,
        /// v0.12.0: back-button hover/pressed for plain page strips.
        hover_back: bool,
        pressed_back: bool,
        /// v0.13.0: close (X) button hover/pressed for plain page strips.
        /// Tabbed strips use hover_tab/pressed_tab with TabHit::CloseWindow.
        hover_close: bool,
        pressed_close: bool,
        mouse_in: bool,
        /// v0.12.0: window title drawn in plain page strips.
        page_title: String,
        /// v0.10.0: tab data for tabbed windows; None for plain page strips.
        tabbed: Option<TabStripData>,
        /// v0.11.0: resolved site tint for plain page strips (None = the
        /// dark default). Tabbed windows use `tabbed.tint` instead.
        page_tint: Option<(u8, u8, u8)>,
        /// v0.10.0: hover/pressed tab-strip hit (TabHit), for painting.
        hover_tab: Option<crate::tabs::TabHit>,
        pressed_tab: Option<crate::tabs::TabHit>,
        /// True while we are moving the caption ourselves (from the owner's
        /// Moved/Resized events): WM_WINDOWPOSCHANGED must not echo the move
        /// back onto the owner.
        syncing: bool,
        last_x: i32,
        last_y: i32,
    }

    struct Chrome {
        app: AppHandle,
        tx: mpsc::Sender<ChromeCmd>,
        wake: HANDLE,
        hook: HHOOK,
        captions: HashMap<String, Caption>,
        /// caption HWND -> page label, for the Esc hook's foreground check.
        by_hwnd: HashMap<isize, String>,
    }

    // HANDLE is a raw pointer (not Send/Sync), but Wake is only ever passed
    // to SetEvent, which is thread-safe by contract.
    #[derive(Clone, Copy)]
    struct Wake(HANDLE);
    unsafe impl Send for Wake {}
    unsafe impl Sync for Wake {}

    thread_local! {
        static CHROME: RefCell<Option<Chrome>> = const { RefCell::new(None) };
    }

    static CHROME_CTL: OnceLock<(mpsc::Sender<ChromeCmd>, Wake)> = OnceLock::new();

    fn with_chrome<R>(f: impl FnOnce(&mut Chrome) -> Option<R>) -> Option<R> {
        CHROME.with(|c| c.borrow_mut().as_mut().and_then(f))
    }

    /// Minimize button: rightmost slot. The maximize/restore button
    /// (v0.9.9) takes the slot to its left; both are BTN_W_LOGICAL wide.
    fn button_rect(hwnd: HWND, scale: f64) -> Option<RECT> {
        button_rect_at(hwnd, scale, 0)
    }

    /// Maximize/restore button: one slot left of minimize.
    fn max_button_rect(hwnd: HWND, scale: f64) -> Option<RECT> {
        button_rect_at(hwnd, scale, 1)
    }

    /// v0.12.0: back button for plain page strips — a left-edge slot the
    /// same size as the window buttons, mirroring button_rect_at.
    fn back_button_rect(hwnd: HWND, scale: f64) -> Option<RECT> {
        unsafe {
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc).ok()?;
            let bw = (BTN_W_LOGICAL * scale).round() as i32;
            Some(RECT {
                left: rc.left,
                top: rc.top,
                right: rc.left + bw,
                bottom: rc.bottom,
            })
        }
    }

    fn button_rect_at(hwnd: HWND, scale: f64, slot: i32) -> Option<RECT> {
        unsafe {
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc).ok()?;
            let bw = (BTN_W_LOGICAL * scale).round() as i32;
            Some(RECT {
                left: rc.right - bw * (slot + 1),
                top: rc.top,
                right: rc.right - bw * slot,
                bottom: rc.bottom,
            })
        }
    }

    fn pt_in_rect(x: i32, y: i32, rc: &RECT) -> bool {
        x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
    }

    fn mouse_xy(lparam: LPARAM) -> (i32, i32) {
        (
            (lparam.0 & 0xffff) as i16 as i32,
            ((lparam.0 >> 16) & 0xffff) as i16 as i32,
        )
    }

    // ------------------------------------------------------------------
    // Caption window proc
    // ------------------------------------------------------------------

    fn hbrush_to_obj(hbr: HBRUSH) -> HGDIOBJ {
        HGDIOBJ(hbr.0)
    }

    unsafe fn paint_caption(hwnd: HWND) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_invalid() {
            return;
        }
        // Snapshot everything the paint needs under one short borrow, then
        // paint with no borrow held (the v0.9.7 two-phase rule).
        let (scale, owner, hover_min, pressed_min, hover_max, pressed_max, tabbed, page_tint, hover_tab, pressed_tab, hover_back, pressed_back, hover_close, pressed_close, page_title) =
            with_chrome(|ch| {
                let label = ch.by_hwnd.get(&(hwnd.0 as isize))?;
                let cp = ch.captions.get(label)?;
                Some((
                    cp.scale,
                    cp.owner,
                    cp.hover_min,
                    cp.pressed_min,
                    cp.hover_max,
                    cp.pressed_max,
                    cp.tabbed.clone(),
                    cp.page_tint,
                    cp.hover_tab,
                    cp.pressed_tab,
                    cp.hover_back,
                    cp.pressed_back,
                    cp.hover_close,
                    cp.pressed_close,
                    cp.page_title.clone(),
                ))
            })
            .unwrap_or((1.0, HWND::default(), false, false, false, false, None, None, None, None, false, false, false, false, String::new()));

        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        // v0.10.0: the tabbed strip paints the active tab's theme-color as
        // its background (the "blend the title bar" request); v0.11.0:
        // plain page strips paint their resolved site tint the same way.
        // Unresolved strips keep the dark default.
        let tint = tabbed.as_ref().and_then(|t| t.tint).or(page_tint);
        let (br, bg_, bb) = tint.unwrap_or((27, 27, 27));
        let bg = CreateSolidBrush(rgb(br, bg_, bb));
        FillRect(hdc, &rc, bg);
        let _ = DeleteObject(hbrush_to_obj(bg));

        // v0.10.0: tab strip for tabbed windows.
        if let Some(td) = tabbed.as_ref() {
            paint_tabs(hdc, hwnd, scale, td, tint, hover_tab, pressed_tab);
        }

        // Maximize/restore button (v0.9.9): one slot left of minimize,
        // same dark styling. Glyph follows the owner's zoom state — a
        // pure query, no messages, safe under the borrow above.
        let zoomed = !owner.is_invalid() && IsZoomed(owner).as_bool();
        if let Some(btn) = max_button_rect(hwnd, scale) {
            if hover_max || pressed_max {
                let bbg = CreateSolidBrush(if pressed_max {
                    rgb(46, 46, 46)
                } else {
                    rgb(58, 58, 58)
                });
                FillRect(hdc, &btn, bbg);
                let _ = DeleteObject(hbrush_to_obj(bbg));
            }
            let glyph = CreateSolidBrush(if hover_max || pressed_max {
                rgb(255, 255, 255)
            } else {
                rgb(204, 204, 204)
            });
            // Outline squares drawn as four bars so no background
            // punch-out is needed (works over the hover highlight).
            let s = (10.0 * scale).round() as i32; // square size
            let t = (2.0 * scale).max(1.0).round() as i32; // bar thickness
            let bw = btn.right - btn.left;
            let bh = btn.bottom - btn.top;
            let square = |left: i32, top: i32| {
                let bars = [
                    RECT { left, top, right: left + s, bottom: top + t },
                    RECT { left, top: top + s - t, right: left + s, bottom: top + s },
                    RECT { left, top: top + t, right: left + t, bottom: top + s - t },
                    RECT { left: left + s - t, top: top + t, right: left + s, bottom: top + s - t },
                ];
                for b in bars {
                    FillRect(hdc, &b, glyph);
                }
            };
            if zoomed {
                // Restore glyph: front square bottom-left, back square
                // peeking top-right.
                let off = (3.0 * scale).round() as i32;
                let cx = btn.left + (bw - s) / 2;
                let cy = btn.top + (bh - s) / 2;
                square(cx + off, cy - off);
                square(cx - off / 2, cy + off / 2);
            } else {
                // Maximize glyph: single square outline, centered.
                square(btn.left + (bw - s) / 2, btn.top + (bh - s) / 2);
            }
            let _ = DeleteObject(hbrush_to_obj(glyph));
        }

        if let Some(btn) = button_rect(hwnd, scale) {
            if hover_min || pressed_min {
                let bbg = CreateSolidBrush(if pressed_min {
                    rgb(46, 46, 46)
                } else {
                    rgb(58, 58, 58)
                });
                FillRect(hdc, &btn, bbg);
                let _ = DeleteObject(hbrush_to_obj(bbg));
            }
            // Minimize glyph: a small horizontal bar, centered.
            let gw = (10.0 * scale).round() as i32;
            let gh = (2.0 * scale).max(1.0).round() as i32;
            let bw = btn.right - btn.left;
            let bh = btn.bottom - btn.top;
            let grc = RECT {
                left: btn.left + (bw - gw) / 2,
                top: btn.top + (bh - gh) / 2,
                right: btn.left + (bw - gw) / 2 + gw,
                bottom: btn.top + (bh - gh) / 2 + gh,
            };
            let glyph = CreateSolidBrush(if hover_min || pressed_min {
                rgb(255, 255, 255)
            } else {
                rgb(204, 204, 204)
            });
            FillRect(hdc, &grc, glyph);
            let _ = DeleteObject(hbrush_to_obj(glyph));
        }
        // v0.10.0: tabbed strips get a window close (X) button in the
        // third slot — an empty group has no tab to close, so the window
        // needs its own X. Plain page strips keep min/max only.
        if tabbed.is_some() {
            paint_x_button(hdc, hwnd, scale, tint, hover_tab, pressed_tab);
        }
        // v0.12.0: plain page strips get a back button and the window
        // title — the strip is a real title bar, not an empty black bar.
        // Tabbed strips already have tabs; they keep their layout.
        if tabbed.is_none() {
            // v0.13.0: close (X) button in the third slot, mirroring the
            // tabbed strip's X: red on hover/press, like the native button.
            if let Some(btn) = button_rect_at(hwnd, scale, 2) {
                if hover_close || pressed_close {
                    let bbg = CreateSolidBrush(if pressed_close {
                        rgb(196, 43, 28)
                    } else {
                        rgb(232, 17, 35)
                    });
                    FillRect(hdc, &btn, bbg);
                    let _ = DeleteObject(hbrush_to_obj(bbg));
                }
                let text_color = if hover_close || pressed_close {
                    rgb(255, 255, 255)
                } else {
                    rgb(204, 204, 204)
                };
                draw_text_centered(hdc, btn, "×", scale, text_color);
            }
            if let Some(btn) = back_button_rect(hwnd, scale) {
                if hover_back || pressed_back {
                    let bbg = CreateSolidBrush(if pressed_back {
                        rgb(46, 46, 46)
                    } else {
                        rgb(58, 58, 58)
                    });
                    FillRect(hdc, &btn, bbg);
                    let _ = DeleteObject(hbrush_to_obj(bbg));
                }
                let text_color = rgb(204, 204, 204);
                draw_text_centered(
                    hdc,
                    btn,
                    "←",
                    scale,
                    if hover_back || pressed_back {
                        rgb(255, 255, 255)
                    } else {
                        text_color
                    },
                );
                // Title: left-aligned after the back button, room reserved
                // for the min/max/close slots on the right.
                if !page_title.is_empty() {
                    let btn_w = btn.right - btn.left;
                    let pad = (8.0 * scale).round() as i32;
                    let mut rc = RECT::default();
                    let _ = GetClientRect(hwnd, &mut rc);
                    let title_rc = RECT {
                        left: rc.left + btn_w + pad,
                        top: rc.top,
                        right: rc.right - btn_w * 3 - pad,
                        bottom: rc.bottom,
                    };
                    if title_rc.right > title_rc.left {
                        draw_text_in(
                            hdc,
                            title_rc,
                            &page_title,
                            scale,
                            text_color,
                            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                        );
                    }
                }
            }
        }
        let _ = EndPaint(hwnd, &ps);
    }

    /// Window close (X) button for tabbed strips: third slot from the
    /// right, drawn as text so no diagonal-bar geometry is needed.
    unsafe fn paint_x_button(
        hdc: HDC,
        hwnd: HWND,
        scale: f64,
        tint: Option<(u8, u8, u8)>,
        hover_tab: Option<crate::tabs::TabHit>,
        pressed_tab: Option<crate::tabs::TabHit>,
    ) {
        use crate::tabs::TabHit;
        let Some(btn) = button_rect_at(hwnd, scale, 2) else {
            return;
        };
        let hovered = hover_tab == Some(TabHit::CloseWindow);
        let pressed = pressed_tab == Some(TabHit::CloseWindow);
        if hovered || pressed {
            // Close hover goes red, like the native button.
            let bbg = CreateSolidBrush(if pressed {
                rgb(196, 43, 28)
            } else {
                rgb(232, 17, 35)
            });
            FillRect(hdc, &btn, bbg);
            let _ = DeleteObject(hbrush_to_obj(bbg));
        }
        let light = tint.map(|(r, g, b)| crate::tabs::tint_wants_light_text(r, g, b));
        let color = match (hovered || pressed, light) {
            (true, _) => rgb(255, 255, 255),
            (false, Some(false)) => rgb(40, 40, 40),
            _ => rgb(204, 204, 204),
        };
        draw_text_centered(hdc, btn, "×", scale, color);
    }

    /// DrawTextW helper: single-line, vertically centered text in a rect.
    /// Selects a Segoe UI font, restores the DC, deletes the font.
    unsafe fn draw_text_centered(hdc: HDC, rc: RECT, text: &str, scale: f64, color: COLORREF) {
        draw_text_in(hdc, rc, text, scale, color, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    }

    unsafe fn draw_text_in(
        hdc: HDC,
        mut rc: RECT,
        text: &str,
        scale: f64,
        color: COLORREF,
        format: DRAW_TEXT_FORMAT,
    ) {
        let height = -((12.0 * scale).round() as i32).max(1);
        let font = CreateFontW(
            height,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            DEFAULT_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            w!("Segoe UI"),
        );
        if font.is_invalid() {
            return;
        }
        let old = SelectObject(hdc, font.into());
        let _ = SetTextColor(hdc, color);
        let _ = SetBkMode(hdc, TRANSPARENT);
        let mut wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        DrawTextW(hdc, &mut wide, &mut rc, format);
        let _ = SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
    }

    /// Paint the tab strip: one tab per entry (active highlighted), a "+"
    /// button after the last tab. Text truncates with an ellipsis.
    unsafe fn paint_tabs(
        hdc: HDC,
        hwnd: HWND,
        scale: f64,
        td: &TabStripData,
        tint: Option<(u8, u8, u8)>,
        hover_tab: Option<crate::tabs::TabHit>,
        pressed_tab: Option<crate::tabs::TabHit>,
    ) {
        use crate::tabs::TabHit;
        use crate::tabs::layout::*;
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let strip_w = rc.right as f64 / scale;
        let light = tint.map(|(r, g, b)| crate::tabs::tint_wants_light_text(r, g, b));
        // Glyph/text colors follow the tint luminance so tabs stay
        // readable on light theme-colors.
        let text_color = match light {
            Some(false) => rgb(35, 35, 35),
            _ => rgb(225, 225, 225),
        };
        let dim_color = match light {
            Some(false) => rgb(90, 90, 90),
            _ => rgb(160, 160, 160),
        };
        let active_bg = match light {
            Some(false) => rgb(255, 255, 255),
            _ => rgb(62, 62, 62),
        };
        let hover_bg = match light {
            Some(false) => rgb(232, 232, 232),
            _ => rgb(46, 46, 46),
        };
        let (ranges, add_x) = tab_ranges(strip_w, td.tabs.len());
        let px = |v: f64| (v * scale).round() as i32;
        for (i, name) in td.tabs.iter().enumerate() {
            let (x0, x1) = ranges[i];
            let active = i == td.active;
            let tab_rc = RECT {
                left: px(x0),
                top: rc.top,
                right: px(x1),
                bottom: rc.bottom,
            };
            let hovered = hover_tab == Some(TabHit::Tab(i))
                || hover_tab == Some(TabHit::CloseTab(i));
            let pressed = pressed_tab == Some(TabHit::Tab(i));
            if active {
                let bbg = CreateSolidBrush(active_bg);
                FillRect(hdc, &tab_rc, bbg);
                let _ = DeleteObject(hbrush_to_obj(bbg));
            } else if hovered || pressed {
                let bbg = CreateSolidBrush(hover_bg);
                FillRect(hdc, &tab_rc, bbg);
                let _ = DeleteObject(hbrush_to_obj(bbg));
            }
            // Tab label: left-padded, room reserved for the close glyph.
            let close_w = px(22.0);
            let text_rc = RECT {
                left: tab_rc.left + px(10.0),
                top: tab_rc.top,
                right: tab_rc.right - close_w,
                bottom: tab_rc.bottom,
            };
            draw_text_in(
                hdc,
                text_rc,
                name,
                scale,
                if active { text_color } else { dim_color },
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
            // Per-tab close glyph, emphasized on hover.
            let x_hovered = hover_tab == Some(TabHit::CloseTab(i));
            let x_rc = RECT {
                left: tab_rc.right - close_w,
                top: tab_rc.top,
                right: tab_rc.right,
                bottom: tab_rc.bottom,
            };
            draw_text_centered(
                hdc,
                x_rc,
                "×",
                scale,
                if x_hovered { text_color } else { dim_color },
            );
        }
        // "+" button after the last tab (at x=0 for an empty group).
        let add_rc = RECT {
            left: px(add_x),
            top: rc.top,
            right: px(add_x + ADD_W),
            bottom: rc.bottom,
        };
        if hover_tab == Some(TabHit::Add) || pressed_tab == Some(TabHit::Add) {
            let bbg = CreateSolidBrush(hover_bg);
            FillRect(hdc, &add_rc, bbg);
            let _ = DeleteObject(hbrush_to_obj(bbg));
        }
        draw_text_centered(hdc, add_rc, "+", scale, dim_color);
    }

    unsafe fn on_lbutton_down(hwnd: HWND, lparam: LPARAM) {
        let (x, y) = mouse_xy(lparam);
        // Decide under the borrow; the HTCAPTION drag starts a modal loop
        // that re-enters caption_proc, so SendMessageW must run AFTER the
        // RefCell borrow is released — never inside it.
        //
        // When the owner is maximized, starting a drag would fight the
        // maximized state: restore first instead (no drag). IsZoomed is a
        // pure query — no messages — so it is safe under the borrow.
        enum Down {
            Drag,
            Restore(HWND),
            // v0.11.2: orphaned strip (owner window dead) — destroy it on
            // click instead of leaving an unclosable ghost on the desktop.
            Drop(String),
            None,
        }
        let down = with_chrome(|ch| {
            let label = ch.by_hwnd.get(&(hwnd.0 as isize))?.clone();
            let cp = ch.captions.get_mut(&label)?;
            // IsWindow is a pure query — no messages — safe under the
            // borrow, like IsZoomed below.
            if !IsWindow(Some(cp.owner)).as_bool() {
                return Some(Down::Drop(label));
            }
            // v0.10.0: tabbed strips hit-test tabs first (logical px).
            // GetClientRect is a pure query — safe under the borrow.
            let tab_hit = cp.tabbed.as_ref().and_then(|td| {
                let mut rc = RECT::default();
                GetClientRect(hwnd, &mut rc).ok()?;
                let scale = cp.scale;
                let strip_w = rc.right as f64 / scale;
                Some(crate::tabs::tab_hit_test(
                    x as f64 / scale,
                    y as f64 / scale,
                    strip_w,
                    td.tabs.len(),
                    22.0,
                ))
            });
            if let Some(hit) = tab_hit {
                use crate::tabs::TabHit;
                match hit {
                    // Tab interactions: press-and-release; the click
                    // dispatches on button-up (see on_lbutton_up).
                    TabHit::Tab(_)
                    | TabHit::CloseTab(_)
                    | TabHit::Add
                    | TabHit::CloseWindow => {
                        cp.pressed_tab = Some(hit);
                        cp.hover_tab = Some(hit);
                        SetCapture(hwnd);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return Some(Down::None);
                    }
                    // Min/max keep the existing button behavior below.
                    TabHit::Min | TabHit::Max | TabHit::Drag => {}
                }
            }
            let over_max =
                max_button_rect(hwnd, cp.scale).is_some_and(|b| pt_in_rect(x, y, &b));
            let over_min =
                button_rect(hwnd, cp.scale).is_some_and(|b| pt_in_rect(x, y, &b));
            // v0.12.0: back button for plain page strips (tabbed strips
            // keep their tab layout).
            let over_back = cp.tabbed.is_none()
                && back_button_rect(hwnd, cp.scale).is_some_and(|b| pt_in_rect(x, y, &b));
            // v0.13.0: close (X) button for plain page strips, third slot.
            let over_close = cp.tabbed.is_none()
                && button_rect_at(hwnd, cp.scale, 2).is_some_and(|b| pt_in_rect(x, y, &b));
            if over_max {
                cp.pressed_max = true;
                SetCapture(hwnd);
            } else if over_min {
                cp.pressed_min = true;
                SetCapture(hwnd);
            } else if over_back {
                cp.pressed_back = true;
                SetCapture(hwnd);
            } else if over_close {
                cp.pressed_close = true;
                SetCapture(hwnd);
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
            if over_max || over_min || over_back || over_close {
                Some(Down::None)
            } else if IsZoomed(cp.owner).as_bool() {
                Some(Down::Restore(cp.owner))
            } else {
                Some(Down::Drag)
            }
        })
        .unwrap_or(Down::None);
        match down {
            Down::Drag => {
                let _ = ReleaseCapture();
                let _ = SendMessageW(
                    hwnd,
                    WM_NCLBUTTONDOWN,
                    Some(WPARAM(HTCAPTION as usize)),
                    Some(LPARAM(0)),
                );
            }
            // Async like the minimize path: no synchronous re-entrancy,
            // so the two-phase rule is satisfied trivially.
            Down::Restore(owner) if !owner.is_invalid() => {
                let _ = PostMessageW(
                    Some(owner),
                    WM_SYSCOMMAND,
                    WPARAM(SC_RESTORE as usize),
                    LPARAM(0),
                );
            }
            // Orphaned strip: destroy it. remove_caption takes only a
            // short borrow and calls DestroyWindow with none held.
            Down::Drop(lbl) => {
                remove_caption(&lbl);
            }
            _ => {}
        }
    }

    unsafe fn on_lbutton_up(hwnd: HWND, lparam: LPARAM) {
        let (x, y) = mouse_xy(lparam);
        let tab_click = with_chrome(|ch| {
            let label = ch.by_hwnd.get(&(hwnd.0 as isize))?.clone();
            let cp = ch.captions.get_mut(&label)?;
            if cp.pressed_max {
                cp.pressed_max = false;
                if GetCapture() == hwnd {
                    let _ = ReleaseCapture();
                }
                let over_max =
                    max_button_rect(hwnd, cp.scale).is_some_and(|b| pt_in_rect(x, y, &b));
                if over_max {
                    // Toggle maximize/restore. PostMessageW is async — like
                    // the minimize path it cannot synchronously re-enter
                    // caption_proc, so the two-phase rule holds without a
                    // second phase. IsZoomed is a pure query (no messages).
                    let cmd = match max_toggle(IsZoomed(cp.owner).as_bool()) {
                        MaxToggle::Maximize => SC_MAXIMIZE,
                        MaxToggle::Restore => SC_RESTORE,
                    };
                    let _ = PostMessageW(
                        Some(cp.owner),
                        WM_SYSCOMMAND,
                        WPARAM(cmd as usize),
                        LPARAM(0),
                    );
                }
            }
            if cp.pressed_min {
                cp.pressed_min = false;
                if GetCapture() == hwnd {
                    let _ = ReleaseCapture();
                }
                let over_button =
                    button_rect(hwnd, cp.scale).is_some_and(|b| pt_in_rect(x, y, &b));
                if over_button {
                    // The owned caption hides with its owner automatically.
                    let _ = PostMessageW(
                        Some(cp.owner),
                        WM_SYSCOMMAND,
                        WPARAM(SC_MINIMIZE as usize),
                        LPARAM(0),
                    );
                }
            }
            // v0.10.0: tab-strip press-and-release. Recompute the hit at
            // release; only a matching press+release dispatches.
            let tab_click = cp.tabbed.as_ref().and_then(|td| {
                let pressed = cp.pressed_tab.take()?;
                if GetCapture() == hwnd {
                    let _ = ReleaseCapture();
                }
                let mut rc = RECT::default();
                GetClientRect(hwnd, &mut rc).ok()?;
                let scale = cp.scale;
                let hit = crate::tabs::tab_hit_test(
                    x as f64 / scale,
                    y as f64 / scale,
                    rc.right as f64 / scale,
                    td.tabs.len(),
                    22.0,
                );
                (hit == pressed).then_some((label.clone(), hit))
            });
            let _ = InvalidateRect(Some(hwnd), None, false);
            Some(tab_click)
        });
        // Dispatch with no borrow held: tab actions build/close windows.
        if let Some(Some((label, hit))) = tab_click {
            dispatch_tab_click(hwnd, label, hit);
        }
        // v0.12.0: back-button press-and-release. Recompute the hit at
        // release; only a matching press+release navigates back.
        let back_click = with_chrome(|ch| {
            let label = ch.by_hwnd.get(&(hwnd.0 as isize))?.clone();
            let cp = ch.captions.get_mut(&label)?;
            if !cp.pressed_back {
                return None;
            }
            cp.pressed_back = false;
            if GetCapture() == hwnd {
                let _ = ReleaseCapture();
            }
            let over =
                back_button_rect(hwnd, cp.scale).is_some_and(|b| pt_in_rect(x, y, &b));
            let _ = InvalidateRect(Some(hwnd), None, false);
            over.then(|| label.clone())
        });
        // Dispatch with no borrow held: the eval goes to the webview.
        if let Some(label) = back_click {
            if let Some(app) = with_chrome(|ch| Some(ch.app.clone())) {
                std::thread::Builder::new()
                    .name("appmaka-strip-back".to_string())
                    .spawn(move || {
                        if let Some(w) = app.get_webview_window(&label) {
                            let _ = w.eval("history.back()");
                        }
                    })
                    .ok();
            }
        }
        // v0.13.0: close (X) button press-and-release for plain page
        // strips. Recompute the hit at release; only a matching
        // press+release closes. User intent: pinned windows get the
        // standard confirm via close_tracked_window.
        let close_click = with_chrome(|ch| {
            let label = ch.by_hwnd.get(&(hwnd.0 as isize))?.clone();
            let cp = ch.captions.get_mut(&label)?;
            if !cp.pressed_close {
                return None;
            }
            cp.pressed_close = false;
            if GetCapture() == hwnd {
                let _ = ReleaseCapture();
            }
            let over =
                button_rect_at(hwnd, cp.scale, 2).is_some_and(|b| pt_in_rect(x, y, &b));
            let _ = InvalidateRect(Some(hwnd), None, false);
            over.then(|| label.clone())
        });
        // Dispatch with no borrow held: closing must not run in the
        // window proc (v0.9.7 re-entrancy rules).
        if let Some(label) = close_click {
            if let Some(app) = with_chrome(|ch| Some(ch.app.clone())) {
                std::thread::Builder::new()
                    .name("appmaka-strip-close".to_string())
                    .spawn(move || {
                        crate::windows::close_tracked_window(
                            &app,
                            &label,
                            crate::pin::CloseIntent::User,
                        );
                    })
                    .ok();
            }
        }
    }

    /// Run a tab-strip click action. Called with no chrome borrow held.
    /// Window building/closing happens on worker threads — never in the
    /// window proc (v0.9.7 re-entrancy rules).
    unsafe fn dispatch_tab_click(hwnd: HWND, label: String, hit: crate::tabs::TabHit) {
        use crate::tabs::TabHit;
        let app = match with_chrome(|ch| Some(ch.app.clone())) {
            Some(a) => a,
            None => return,
        };
        let spawn_switch = |app: AppHandle, label: String, index: usize| {
            std::thread::Builder::new()
                .name("appmaka-tab-click".to_string())
                .spawn(move || {
                    let (Some(ts), Some(store), Some(adblock)) = (
                        app.try_state::<crate::tabs::TabState>(),
                        app.try_state::<crate::store::AppStore>(),
                        app.try_state::<crate::adblock::AdblockState>(),
                    ) else {
                        return;
                    };
                    if let Some(gid) = crate::tabs::group_id_for_label(&app, &label) {
                        let _ = crate::tabs::switch_tab(&app, &store, &adblock, &ts, &gid, index);
                    }
                })
                .ok();
        };
        match hit {
            TabHit::Tab(i) => spawn_switch(app, label, i),
            TabHit::CloseTab(i) => {
                std::thread::Builder::new()
                    .name("appmaka-tab-close".to_string())
                    .spawn(move || {
                        let (Some(ts), Some(store), Some(adblock)) = (
                            app.try_state::<crate::tabs::TabState>(),
                            app.try_state::<crate::store::AppStore>(),
                            app.try_state::<crate::adblock::AdblockState>(),
                        ) else {
                            return;
                        };
                        if let Some(gid) = crate::tabs::group_id_for_label(&app, &label) {
                            let _ = crate::tabs::close_tab(&app, &store, &adblock, &ts, &gid, i);
                        }
                    })
                    .ok();
            }
            TabHit::Add => show_add_tab_menu(&app, &label),
            TabHit::CloseWindow => crate::tabs::request_close_group_by_label(&app, &label),
            TabHit::Min | TabHit::Max | TabHit::Drag => {}
        }
        let _ = hwnd;
    }

    /// "+" picker: native popup menu listing every app/account (two-phase
    /// like the snap menu — snapshot under no borrow, modal menu after).
    /// The choice fans out to tabs::add_tab on a worker thread.
    unsafe fn show_add_tab_menu(app: &AppHandle, label: &str) {
        struct PickItem {
            app_id: String,
            account_id: String,
            display: String,
        }
        let items: Vec<PickItem> = app
            .try_state::<crate::store::AppStore>()
            .and_then(|s| s.list().ok())
            .unwrap_or_default()
            .into_iter()
            .flat_map(|a| {
                let multi = a.accounts.len() > 1;
                let name = a.name.clone();
                let id = a.id.clone();
                a.accounts.into_iter().map(move |ac| PickItem {
                    app_id: id.clone(),
                    account_id: ac.id.clone(),
                    display: if multi {
                        format!("{} — {}", name, ac.label)
                    } else {
                        name.clone()
                    },
                })
            })
            .collect();
        if items.is_empty() {
            return;
        }
        let menu = CreatePopupMenu().unwrap_or_default();
        if menu.is_invalid() {
            return;
        }
        for (i, item) in items.iter().enumerate() {
            let mut wide: Vec<u16> = OsStr::new(&item.display)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // AppendMenuW copies the string synchronously.
            let _ = AppendMenuW(menu, MF_STRING, i + 1, PWSTR(wide.as_mut_ptr()));
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // The strip stays alive for the modal menu: it is an owned window
        // of the page, which outlives the menu either way.
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD, pt.x, pt.y, Some(0), GetForegroundWindow(), None);
        let _ = DestroyMenu(menu);
        if !cmd.as_bool() {
            return;
        }
        let Some(pick) = items.get(cmd.0 as usize - 1) else {
            return;
        };
        let app = app.clone();
        let label = label.to_string();
        let (app_id, account_id) = (pick.app_id.clone(), pick.account_id.clone());
        std::thread::Builder::new()
            .name("appmaka-tab-add".to_string())
            .spawn(move || {
                let (Some(ts), Some(store), Some(adblock)) = (
                    app.try_state::<crate::tabs::TabState>(),
                    app.try_state::<crate::store::AppStore>(),
                    app.try_state::<crate::adblock::AdblockState>(),
                ) else {
                    return;
                };
                if let Some(gid) = crate::tabs::group_id_for_label(&app, &label) {
                    let _ =
                        crate::tabs::add_tab(&app, &store, &adblock, &ts, &gid, &app_id, &account_id);
                }
            })
            .ok();
    }

    /// Right-click on the maximize/restore button: our own minimal snap
    /// menu (v0.9.9). The native Windows 11 snap-layouts flyout is not
    /// reachable from our architecture (see SnapZone docs), so this native
    /// popup menu is the honest fallback.
    ///
    /// Two-phase: TrackPopupMenu runs a modal loop that synchronously
    /// dispatches to caption_proc, so phase 1 only snapshots (owner +
    /// monitor work rect — pure queries, no messages) under a short
    /// borrow, and phase 2 builds the menu, runs it, and moves the owner
    /// with no borrow held.
    unsafe fn on_rbutton_up(hwnd: HWND, lparam: LPARAM) {
        let (x, y) = mouse_xy(lparam);
        struct SnapPlan {
            owner: HWND,
            work: RECT,
        }
        let plan = with_chrome(|ch| {
            let label = ch.by_hwnd.get(&(hwnd.0 as isize))?.clone();
            let cp = ch.captions.get(&label)?;
            let over_max =
                max_button_rect(hwnd, cp.scale).is_some_and(|b| pt_in_rect(x, y, &b));
            if !over_max || !IsWindow(Some(cp.owner)).as_bool() {
                return None;
            }
            let hmon = MonitorFromWindow(cp.owner, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
                return None;
            }
            Some(SnapPlan {
                owner: cp.owner,
                work: mi.rcWork,
            })
        });
        let Some(plan) = plan else {
            return;
        };
        // Phase 2: no borrow held from here on.
        let menu = CreatePopupMenu().unwrap_or_default();
        if menu.is_invalid() {
            return;
        }
        for (i, zone) in SnapZone::menu_order().into_iter().enumerate() {
            let mut wide: Vec<u16> = OsStr::new(zone.menu_label())
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // AppendMenuW copies the string synchronously; `wide` only
            // needs to live for the call.
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                i + 1,
                PWSTR(wide.as_mut_ptr()),
            );
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD, pt.x, pt.y, Some(0), hwnd, None);
        let _ = DestroyMenu(menu);
        // With TPM_RETURNCMD the BOOL carries the chosen item id; 0/false
        // means dismissed.
        if !cmd.as_bool() {
            return;
        }
        let Some(zone) = SnapZone::menu_order()
            .get(cmd.0 as usize - 1)
            .copied()
        else {
            return;
        };
        let (zx, zy, zw, zh) = snap_zone_rect(
            (
                plan.work.left,
                plan.work.top,
                plan.work.right,
                plan.work.bottom,
            ),
            zone,
        );
        // A maximized window keeps its maximized state across SetWindowPos;
        // restore first so the zone rect takes effect. No borrow held.
        if IsZoomed(plan.owner).as_bool() {
            let _ = ShowWindow(plan.owner, SW_RESTORE);
        }
        let _ = SetWindowPos(
            plan.owner,
            None,
            zx,
            zy,
            zw,
            zh,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }

    unsafe fn on_mouse_move(hwnd: HWND, lparam: LPARAM) {
        let (x, y) = mouse_xy(lparam);
        with_chrome(|ch| {
            let label = ch.by_hwnd.get(&(hwnd.0 as isize))?.clone();
            let cp = ch.captions.get_mut(&label)?;
            if !cp.mouse_in {
                cp.mouse_in = true;
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
            let hover_min =
                button_rect(hwnd, cp.scale).is_some_and(|b| pt_in_rect(x, y, &b));
            let hover_max =
                max_button_rect(hwnd, cp.scale).is_some_and(|b| pt_in_rect(x, y, &b));
            // v0.12.0: back-button hover for plain page strips.
            let hover_back = cp.tabbed.is_none()
                && back_button_rect(hwnd, cp.scale).is_some_and(|b| pt_in_rect(x, y, &b));
            // v0.13.0: close (X) hover for plain page strips.
            let hover_close = cp.tabbed.is_none()
                && button_rect_at(hwnd, cp.scale, 2).is_some_and(|b| pt_in_rect(x, y, &b));
            if hover_min != cp.hover_min
                || hover_max != cp.hover_max
                || hover_back != cp.hover_back
                || hover_close != cp.hover_close
            {
                cp.hover_min = hover_min;
                cp.hover_max = hover_max;
                cp.hover_back = hover_back;
                cp.hover_close = hover_close;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            // v0.10.0: tab hover for tabbed strips (pure query under the
            // borrow, invalidate only on change).
            let hover_tab = cp.tabbed.as_ref().and_then(|td| {
                let mut rc = RECT::default();
                GetClientRect(hwnd, &mut rc).ok()?;
                let scale = cp.scale;
                let hit = crate::tabs::tab_hit_test(
                    x as f64 / scale,
                    y as f64 / scale,
                    rc.right as f64 / scale,
                    td.tabs.len(),
                    22.0,
                );
                use crate::tabs::TabHit;
                match hit {
                    TabHit::Drag | TabHit::Min | TabHit::Max => None,
                    h => Some(h),
                }
            });
            if hover_tab != cp.hover_tab {
                cp.hover_tab = hover_tab;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            Some(())
        });
    }

    unsafe fn on_mouse_leave(hwnd: HWND) {
        with_chrome(|ch| {
            let label = ch.by_hwnd.get(&(hwnd.0 as isize))?.clone();
            let cp = ch.captions.get_mut(&label)?;
            cp.mouse_in = false;
            cp.hover_min = false;
            cp.hover_max = false;
            cp.hover_back = false;
            cp.hover_close = false;
            cp.hover_tab = None;
            let _ = InvalidateRect(Some(hwnd), None, false);
            Some(())
        });
    }

    /// The caption moved (user drag): move the owner by the same delta,
    /// keeping the strip pinned on-screen (it is the window's only drag
    /// handle and its caption buttons).
    ///
    /// Two phases: SetWindowPos on our OWN window delivers
    /// WM_WINDOWPOSCHANGED synchronously (re-entrant caption_proc), so no
    /// window call may run while the RefCell is borrowed. Phase 1 decides
    /// under a short read-only borrow; phase 2 acts with it released.
    unsafe fn on_pos_changed(hwnd: HWND) {
        enum Act {
            Skip,
            /// Nudge the strip itself to y=0, then shift the owner by the
            /// residual delta (matches the old single-pass behavior).
            Nudge { x: i32 },
            /// Shift the owner by the drag delta.
            Shift { dx: i32, dy: i32 },
        }
        struct Plan {
            label: String,
            act: Act,
            /// Caption rect read in phase 1; becomes last_x/last_y.
            last: (i32, i32),
        }
        let plan = with_chrome(|ch| {
            let label = ch.by_hwnd.get(&(hwnd.0 as isize))?.clone();
            let cp = ch.captions.get(&label)?;
            if cp.syncing {
                return Some(Plan {
                    label,
                    act: Act::Skip,
                    last: (0, 0),
                });
            }
            let mut rc = RECT::default();
            GetWindowRect(hwnd, &mut rc).ok()?;
            let act = if rc.top < 0 {
                Act::Nudge { x: rc.left }
            } else {
                let (dx, dy) = (rc.left - cp.last_x, rc.top - cp.last_y);
                if dx != 0 || dy != 0 {
                    Act::Shift { dx, dy }
                } else {
                    Act::Skip
                }
            };
            Some(Plan {
                label,
                act,
                last: (rc.left, rc.top),
            })
        });
        let plan = match plan {
            Some(p) => p,
            None => return,
        };
        match plan.act {
            Act::Skip => {}
            Act::Nudge { x } => {
                // Flag syncing across the synchronous echo from our own
                // SetWindowPos so the re-entrant call ignores it.
                with_chrome(|ch| {
                    if let Some(cp) = ch.captions.get_mut(&plan.label) {
                        cp.syncing = true;
                    }
                    Some(())
                });
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    x,
                    0,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                // Decide the owner follow-up under a short borrow. Moving
                // the owner moves the owned strip with it, which echoes a
                // synchronous WM_WINDOWPOSCHANGED back into caption_proc —
                // so the SetWindowPos runs after the borrow is released,
                // guarded by syncing like the strip nudge above. (Before
                // the structural fix this echo hit borrow_mut on the held
                // borrow; the panic was contained but the echo was lost.
                // The flag preserves the behavior without the panic.)
                let follow: Option<(HWND, i32, i32)> = with_chrome(|ch| {
                    let cp = ch.captions.get_mut(&plan.label)?;
                    let mut rc = RECT::default();
                    GetWindowRect(hwnd, &mut rc).ok()?;
                    let (dx, dy) = (rc.left - cp.last_x, rc.top - cp.last_y);
                    cp.last_x = rc.left;
                    cp.last_y = rc.top;
                    let mut orc = RECT::default();
                    if (dx != 0 || dy != 0) && GetWindowRect(cp.owner, &mut orc).is_ok() {
                        cp.syncing = true;
                        Some(Some((cp.owner, orc.left + dx, orc.top + dy)))
                    } else {
                        cp.syncing = false;
                        Some(None)
                    }
                })
                .flatten();
                if let Some((owner, ox, oy)) = follow {
                    let _ = SetWindowPos(
                        owner,
                        None,
                        ox,
                        oy,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    with_chrome(|ch| {
                        if let Some(cp) = ch.captions.get_mut(&plan.label) {
                            cp.syncing = false;
                        }
                        Some(())
                    });
                }
            }
            Act::Shift { dx, dy } => {
                let follow: Option<(HWND, i32, i32)> = with_chrome(|ch| {
                    let cp = ch.captions.get_mut(&plan.label)?;
                    cp.last_x = plan.last.0;
                    cp.last_y = plan.last.1;
                    let mut orc = RECT::default();
                    if GetWindowRect(cp.owner, &mut orc).is_ok() {
                        // Guard the synchronous echo (owned strip follows
                        // its owner) with syncing; cleared after the move.
                        cp.syncing = true;
                        Some(Some((cp.owner, orc.left + dx, orc.top + dy)))
                    } else {
                        Some(None)
                    }
                })
                .flatten();
                if let Some((owner, ox, oy)) = follow {
                    let _ = SetWindowPos(
                        owner,
                        None,
                        ox,
                        oy,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    with_chrome(|ch| {
                        if let Some(cp) = ch.captions.get_mut(&plan.label) {
                            cp.syncing = false;
                        }
                        Some(())
                    });
                }
            }
        }
    }

    unsafe fn on_dpi_changed(hwnd: HWND, wparam: WPARAM) {
        let dpi = (wparam.0 & 0xffff) as u32;
        if dpi == 0 {
            return;
        }
        // Update the scale under a short borrow, then reposition with no
        // borrow held: reposition_caption's SetWindowPos calls re-enter
        // caption_proc synchronously (the v0.9.6 P1 abort).
        let label = with_chrome(|ch| {
            let label = ch.by_hwnd.get(&(hwnd.0 as isize))?.clone();
            let cp = ch.captions.get_mut(&label)?;
            cp.scale = dpi as f64 / 96.0;
            Some(label)
        });
        if let Some(label) = label {
            reposition_caption(&label);
        }
    }

    unsafe extern "system" fn caption_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // Never unwind across the FFI boundary: a panic in a window proc
        // aborts the process. On panic, fall through to DefWindowProcW.
        // (This also contains the two known re-entrant paths — the modal
        // HTCAPTION drag loop and the self-nudge SetWindowPos — which are
        // additionally restructured below to not need it.)
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            caption_proc_inner(hwnd, msg, wparam, lparam)
        })) {
            Ok(lr) => lr,
            Err(_) => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn caption_proc_inner(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_PAINT => {
                paint_caption(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                on_lbutton_down(hwnd, lparam);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                on_lbutton_up(hwnd, lparam);
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                on_rbutton_up(hwnd, lparam);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                on_mouse_move(hwnd, lparam);
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                on_mouse_leave(hwnd);
                LRESULT(0)
            }
            WM_WINDOWPOSCHANGED => {
                on_pos_changed(hwnd);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                on_dpi_changed(hwnd, wparam);
                LRESULT(0)
            }
            WM_DESTROY => {
                // The owner is going away (owned windows die with it): drop
                // the bookkeeping so a later RemoveCaption is a no-op.
                with_chrome(|ch| {
                    if let Some(label) = ch.by_hwnd.remove(&(hwnd.0 as isize)) {
                        ch.captions.remove(&label);
                    }
                    maybe_uninstall_esc_hook(ch);
                    Some(())
                });
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn register_class() {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(caption_proc),
            hInstance: HINSTANCE(hinstance.0),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("AppMakaCaption"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }

    /// Quiet discoverability for the close gesture: a standard tooltip over
    /// the strip. TTF_SUBCLASS relays the mouse messages for us.
    unsafe fn create_tooltip(parent: HWND) {
        let tip = match CreateWindowExW(
            WS_EX_TOPMOST,
            TOOLTIPS_CLASSW,
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            Some(parent),
            None,
            None,
            None,
        ) {
            Ok(h) => h,
            Err(_) => return,
        };
        let text = "Hold the left mouse button and press Esc to close this window. Right-click the maximize button for snap layouts.";
        let mut wide: Vec<u16> = OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect();
        let mut rc = RECT::default();
        let _ = GetClientRect(parent, &mut rc);
        let mut ti = TTTOOLINFOW {
            cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
            uFlags: TTF_SUBCLASS | TTF_IDISHWND,
            hwnd: parent,
            uId: parent.0 as usize,
            rect: rc,
            hinst: HINSTANCE::default(),
            lpszText: PWSTR(wide.as_mut_ptr()),
            lParam: LPARAM(0),
            lpReserved: std::ptr::null_mut(),
        };
        SendMessageW(
            tip,
            TTM_ADDTOOLW,
            Some(WPARAM(0)),
            Some(LPARAM(&mut ti as *mut TTTOOLINFOW as isize)),
        );
        SendMessageW(tip, TTM_SETMAXTIPWIDTH, Some(WPARAM(0)), Some(LPARAM(320)));
    }

    /// Place the strip directly above its owner. A maximized owner keeps
    /// the strip, pinned to the top of the monitor work area, so the
    /// restore button stays clickable — hiding it would make maximize a
    /// one-way trap for the mouse.
    ///
    /// Two-phase throughout: ShowWindow/SetWindowPos deliver messages
    /// synchronously and re-enter caption_proc, so no borrow is held
    /// across them. Phase 1 snapshots under a short borrow (IsWindow /
    /// IsZoomed / GetWindowRect / MonitorFromWindow / GetMonitorInfoW are
    /// pure queries — they deliver no messages); phase 2 acts with the
    /// borrow released; phase 3 records the result under a short borrow.
    unsafe fn reposition_caption(label: &str) {
        struct Snap {
            hwnd: HWND,
            owner: HWND,
        }
        enum Plan {
            Skip,
            // v0.11.2: owner window is dead — the strip is an orphaned
            // ghost. Destroy it rather than leaving it stranded visible.
            Drop,
            Place {
                snap: Snap,
                tx: i32,
                ty: i32,
                w: i32,
                h: i32,
            },
        }
        let plan = with_chrome(|ch| {
            let cp = ch.captions.get(label)?;
            if !IsWindow(Some(cp.owner)).as_bool() {
                return Some(Plan::Drop);
            }
            let h = (BAR_H_LOGICAL * cp.scale).round() as i32;
            if IsZoomed(cp.owner).as_bool() {
                // Maximized: GetWindowRect bleeds past the monitor by the
                // (hidden) border size, so anchor to the work area instead.
                let hmon = MonitorFromWindow(cp.owner, MONITOR_DEFAULTTONEAREST);
                let mut mi = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
                    return Some(Plan::Skip);
                }
                let w = mi.rcWork.right - mi.rcWork.left;
                return Some(Plan::Place {
                    snap: Snap {
                        hwnd: cp.hwnd,
                        owner: cp.owner,
                    },
                    tx: mi.rcWork.left,
                    ty: mi.rcWork.top,
                    w,
                    h,
                });
            }
            let mut orc = RECT::default();
            if GetWindowRect(cp.owner, &mut orc).is_err() {
                return Some(Plan::Skip);
            }
            let w = orc.right - orc.left;
            Some(Plan::Place {
                snap: Snap {
                    hwnd: cp.hwnd,
                    owner: cp.owner,
                },
                tx: orc.left,
                ty: orc.top - h,
                w,
                h,
            })
        });
        let plan = match plan {
            Some(p) => p,
            None => return,
        };
        match plan {
            Plan::Skip => {}
            Plan::Drop => {
                remove_caption(label);
            }
            Plan::Place { snap, tx, ty, w, h } => {
                let _ = ShowWindow(snap.hwnd, SW_SHOWNOACTIVATE);
                // Never strand the strip off the top of the screen: nudge
                // the owner down so the strip fits, then re-read.
                let (tx, ty) = if ty < 0 {
                    let _ = SetWindowPos(
                        snap.owner,
                        None,
                        tx,
                        h,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    let mut orc2 = RECT::default();
                    if GetWindowRect(snap.owner, &mut orc2).is_err() {
                        return;
                    }
                    (orc2.left, orc2.top - h)
                } else {
                    (tx, ty)
                };
                let mut crc = RECT::default();
                let same = GetWindowRect(snap.hwnd, &mut crc).is_ok()
                    && crc.left == tx
                    && crc.top == ty
                    && crc.right - crc.left == w
                    && crc.bottom - crc.top == h;
                if !same {
                    // Flag syncing across our own SetWindowPos so the
                    // re-entrant on_pos_changed ignores the echo.
                    with_chrome(|ch| {
                        if let Some(cp) = ch.captions.get_mut(label) {
                            cp.syncing = true;
                        }
                        Some(())
                    });
                    let _ = SetWindowPos(
                        snap.hwnd,
                        None,
                        tx,
                        ty,
                        w,
                        h,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                with_chrome(|ch| {
                    let cp = ch.captions.get_mut(label)?;
                    cp.syncing = false;
                    cp.last_x = tx;
                    cp.last_y = ty;
                    Some(())
                });
            }
        }
    }

    /// Create the strip for a page window. Three phases: the
    /// CreateWindowExW call runs with NO borrow held — it synchronously
    /// delivers WM_NCCREATE / WM_CREATE / WM_SIZE / WM_WINDOWPOSCHANGED to
    /// caption_proc, and holding the RefCell across it was the v0.9.6
    /// crash (re-entrant borrow_mut → panic → unwind across extern
    /// "system" → instant process abort).
    unsafe fn create_caption(
        owner: HWND,
        label: &str,
        scale: f64,
        tabbed: Option<TabStripData>,
        page_title: String,
    ) {
        // Phase 1: duplicate check under a short borrow.
        let exists = with_chrome(|ch| Some(ch.captions.contains_key(label))).unwrap_or(false);
        if exists {
            reposition_caption(label);
            return;
        }
        let h = (BAR_H_LOGICAL * scale).round() as i32;
        // Phase 2: NO borrow held across this call.
        // WS_POPUP with an owner HWND: an *owned* window — always above its
        // owner in z-order, hidden with it on minimize, no taskbar button.
        // WS_EX_NOACTIVATE keeps page focus when the strip is clicked.
        let hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("AppMakaCaption"),
            w!(""),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            100,
            h,
            Some(owner),
            None,
            None,
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(e) => {
                if let Some(app) = with_chrome(|ch| Some(ch.app.clone())) {
                    caption_log(&app, &format!("couldn't create strip for '{label}': {e}"));
                }
                return;
            }
        };
        // v0.13.0: round the strip's corners (Windows 11 style) so they
        // blend with the page window below. No borrow held here (Phase 2);
        // failure just leaves square corners, never a crash.
        let round = DWMWCP_ROUND.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &round as *const i32 as *const std::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
        create_tooltip(hwnd);
        // Phase 3: register under a short borrow.
        with_chrome(|ch| {
            ch.by_hwnd.insert(hwnd.0 as isize, label.to_string());
            ch.captions.insert(
                label.to_string(),
                Caption {
                    hwnd,
                    owner,
                    scale,
                    hover_min: false,
                    pressed_min: false,
                    hover_max: false,
                    pressed_max: false,
                    hover_back: false,
                    pressed_back: false,
                    hover_close: false,
                    pressed_close: false,
                    mouse_in: false,
                    page_title,
                    tabbed,
                    page_tint: None,
                    hover_tab: None,
                    pressed_tab: None,
                    syncing: false,
                    last_x: 0,
                    last_y: 0,
                },
            );
            Some(())
        });
        // Position and hook with no borrow held (both manage their own
        // short borrows internally).
        reposition_caption(label);
        ensure_esc_hook();
    }

    /// Drop the strip's bookkeeping, then destroy the window with no
    /// borrow held: DestroyWindow synchronously delivers WM_DESTROY /
    /// WM_NCDESTROY to caption_proc, whose handler takes the borrow.
    unsafe fn remove_caption(label: &str) {
        let hwnd = with_chrome(|ch| {
            let cp = ch.captions.remove(label)?;
            ch.by_hwnd.remove(&(cp.hwnd.0 as isize));
            maybe_uninstall_esc_hook(ch);
            Some(cp.hwnd)
        });
        if let Some(hwnd) = hwnd {
            if IsWindow(Some(hwnd)).as_bool() {
                let _ = DestroyWindow(hwnd);
            }
        }
    }

    // ------------------------------------------------------------------
    // Esc+LMB close gesture: WH_KEYBOARD_LL on the chrome thread
    // ------------------------------------------------------------------

    /// Install the Esc-gesture hook if none is installed. Manages its own
    /// short borrows: installing the hook can deliver callbacks on this
    /// thread, so no borrow is held across SetWindowsHookExW.
    unsafe fn ensure_esc_hook() {
        let app = with_chrome(|ch| {
            if ch.hook.is_invalid() {
                Some(ch.app.clone())
            } else {
                None
            }
        });
        let Some(app) = app else { return };
        match SetWindowsHookExW(WH_KEYBOARD_LL, Some(esc_proc), None, 0) {
            Ok(hook) => {
                with_chrome(|ch| {
                    if ch.hook.is_invalid() {
                        ch.hook = hook;
                    } else {
                        // A re-entrant path installed one first; drop ours.
                        let _ = UnhookWindowsHookEx(hook);
                    }
                    Some(())
                });
            }
            Err(e) => caption_log(&app, &format!("Esc-gesture hook failed: {e}")),
        }
    }

    unsafe fn maybe_uninstall_esc_hook(ch: &mut Chrome) {
        if ch.captions.is_empty() && !ch.hook.is_invalid() {
            let _ = UnhookWindowsHookEx(ch.hook);
            ch.hook = HHOOK::default();
        }
    }

    /// Foreground HWND -> page label, if it is one of ours: a caption strip
    /// resolves to its page, a page window to itself. Uses try_borrow:
    /// the hook proc runs on the chrome thread and must never panic on a
    /// contended borrow — on contention the gesture just passes through.
    fn resolve_page_label(fg: HWND) -> Option<String> {
        CHROME.with(|c| {
            let ch = c.try_borrow().ok()?;
            let ch = ch.as_ref()?;
            if let Some(label) = ch.by_hwnd.get(&(fg.0 as isize)) {
                return Some(label.clone());
            }
            ch.captions
                .iter()
                .find(|(_, cp)| cp.owner == fg)
                .map(|(label, _)| label.clone())
        })
    }

    unsafe extern "system" fn esc_proc(
        n_code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // Never unwind across the FFI boundary: a panic in a hook proc
        // aborts the process. Fail open — pass the key through.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            esc_proc_inner(n_code, wparam, lparam)
        })) {
            Ok(lr) => lr,
            Err(_) => CallNextHookEx(None, n_code, wparam, lparam),
        }
    }

    unsafe fn esc_proc_inner(n_code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if n_code >= 0 {
            let w = wparam.0 as u32;
            if w == WM_KEYDOWN || w == WM_SYSKEYDOWN {
                let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
                if kb.vkCode == VK_ESCAPE_U32 {
                    let lmb_held = (GetAsyncKeyState(VK_LBUTTON_I32) as u16 & 0x8000) != 0;
                    let label = resolve_page_label(GetForegroundWindow());
                    let g = EscGesture {
                        esc_down: true,
                        lmb_held,
                        foreground_is_page: label.is_some(),
                    };
                    if esc_gesture_closes(g) {
                        if let Some(label) = label {
                            CHROME.with(|c| {
                                // try_borrow, never borrow: on contention
                                // the gesture just passes through instead
                                // of panicking inside the hook proc.
                                let guard = c.try_borrow().ok();
                                if let Some(ch) =
                                    guard.as_ref().and_then(|g| g.as_ref())
                                {
                                    let _ = ch.tx.send(ChromeCmd::ClosePage { label });
                                    let _ = SetEvent(ch.wake);
                                }
                            });
                        }
                        // Swallow: the page never sees the Esc that closed it.
                        return LRESULT(1);
                    }
                }
                // v0.10.0: Ctrl+Tab / Ctrl+Shift+Tab / Ctrl+1..9 switches
                // tabs, but ONLY when a tabbed window (or its strip) is in
                // the foreground — everywhere else the keys pass through
                // untouched (browsers keep their own Ctrl+Tab).
                const VK_TAB_U32: u32 = 0x09;
                const VK_CONTROL_I32: i32 = 0x11;
                const VK_SHIFT_I32: i32 = 0x10;
                if kb.vkCode == VK_TAB_U32 || (0x31..=0x39).contains(&kb.vkCode) {
                    let ctrl =
                        (GetAsyncKeyState(VK_CONTROL_I32) as u16 & 0x8000) != 0;
                    let shift =
                        (GetAsyncKeyState(VK_SHIFT_I32) as u16 & 0x8000) != 0;
                    if let Some(action) = crate::tabs::tab_key_action(kb.vkCode, ctrl, shift) {
                        if let Some(label) = resolve_page_label(GetForegroundWindow()) {
                            if crate::tabs::is_tabbed_label(&label) {
                                CHROME.with(|c| {
                                    let guard = c.try_borrow().ok();
                                    if let Some(ch) =
                                        guard.as_ref().and_then(|g| g.as_ref())
                                    {
                                        let _ = ch.tx.send(ChromeCmd::TabKey { label, action });
                                        let _ = SetEvent(ch.wake);
                                    }
                                });
                                // Swallow: the page never sees the tab-switch keys.
                                return LRESULT(1);
                            }
                        }
                    }
                }
            }
        }
        CallNextHookEx(None, n_code, wparam, lparam)
    }

    // ------------------------------------------------------------------
    // Chrome thread
    // ------------------------------------------------------------------

    /// One chrome-thread command. Runs under catch_unwind at the call
    /// site: a panicking command must never kill the chrome thread (that
    /// would orphan every strip and the gesture hook).
    unsafe fn handle_chrome_cmd(cmd: ChromeCmd) {
        match cmd {
            ChromeCmd::AddCaption {
                owner,
                label,
                scale,
                title,
            } => {
                // create_caption manages its own short borrows: the
                // CreateWindowExW call must run with no borrow held.
                create_caption(HWND(owner as *mut _), &label, scale, None, title);
            }
            ChromeCmd::RemoveCaption { label } => {
                remove_caption(&label);
            }
            ChromeCmd::Reposition { label } => {
                reposition_caption(&label);
            }
            ChromeCmd::ClosePage { label } => {
                // Borrow only to fetch the app handle: close() can
                // synchronously destroy the owned caption (WM_DESTROY runs
                // on this thread), which must not happen under our borrow.
                // The v0.9.9 pin check takes only a short mutex lock — no
                // Win32 call under it (v0.9.7 rules).
                //
                // v0.10.0: in a tabbed window the Esc+LMB gesture closes
                // the ACTIVE TAB (last tab closes the window, pin-aware),
                // not the whole window. The tab close may block on the pin
                // confirm, so it runs on a worker thread — never on the
                // chrome thread's message pump.
                if crate::tabs::is_tabbed_label(&label) {
                    let app = CHROME.with(|c| {
                        c.borrow().as_ref().map(|ch| ch.app.clone())
                    });
                    if let Some(app) = app {
                        std::thread::Builder::new()
                            .name("appmaka-tab-gesture".to_string())
                            .spawn(move || {
                                crate::tabs::gesture_close_active_tab(&app, &label)
                            })
                            .ok();
                    }
                } else {
                    let ctx = CHROME.with(|c| {
                        c.borrow().as_ref().map(|ch| {
                            (ch.app.clone(), ch.app.get_webview_window(&label))
                        })
                    });
                    if let Some((app, Some(w))) = ctx {
                        if crate::pin::is_pinned(&app, &label) {
                            // Pinned ("Don't close this window"): the
                            // hold-LEFT+Esc gesture asks instead of closing.
                            // ask_then_close shows the native dialog on its own
                            // thread — safe from this worker thread.
                            crate::pin::ask_then_close(&app, &label);
                        } else {
                            let _ = w.close();
                        }
                    }
                }
            }
            // v0.10.0: tabbed-window commands.
            ChromeCmd::AddTabbedCaption {
                owner,
                label,
                scale,
                tabs,
            } => {
                create_caption(
                    HWND(owner as *mut _),
                    &label,
                    scale,
                    Some(tabs),
                    String::new(),
                );
            }
            ChromeCmd::UpdateTabbedTabs { label, tabs } => {
                // Mutate under a short borrow, invalidate after it is
                // released (two-phase discipline).
                let hwnd = with_chrome(|ch| {
                    let cp = ch.captions.get_mut(&label)?;
                    cp.tabbed = Some(tabs);
                    Some(cp.hwnd)
                });
                if let Some(hwnd) = hwnd {
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            ChromeCmd::SetTabTint { label, tint } => {
                if let Some(hwnd) = with_chrome(|ch| {
                    let cp = ch.captions.get_mut(&label)?;
                    if let Some(td) = cp.tabbed.as_mut() {
                        td.tint = tint;
                    }
                    Some(cp.hwnd)
                }) {
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            ChromeCmd::SetPageTint { label, tint } => {
                // Same two-phase discipline: mutate under a short borrow,
                // invalidate after it is released.
                let hwnd = with_chrome(|ch| {
                    let cp = ch.captions.get_mut(&label)?;
                    // Plain strips only: tabbed windows own their tint via
                    // SetTabTint (the active tab's color).
                    if cp.tabbed.is_none() {
                        cp.page_tint = tint;
                    }
                    Some(cp.hwnd)
                });
                if let Some(hwnd) = hwnd {
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
            }
            ChromeCmd::TabKey { label, action } => {
                // Ctrl+Tab et al: resolve the target tab and switch on a
                // worker thread (window building never on the chrome
                // thread, and switch_tab may briefly block on locks).
                let app = CHROME.with(|c| {
                    c.borrow().as_ref().map(|ch| ch.app.clone())
                });
                if let Some(app) = app {
                    std::thread::Builder::new()
                        .name("appmaka-tab-key".to_string())
                        .spawn(move || {
                            let (Some(ts), Some(store), Some(adblock)) = (
                                app.try_state::<crate::tabs::TabState>(),
                                app.try_state::<crate::store::AppStore>(),
                                app.try_state::<crate::adblock::AdblockState>(),
                            ) else {
                                return;
                            };
                            let target = crate::tabs::resolve_key_switch(&ts, &label, action);
                            if let Some((gid, idx)) = target {
                                let _ = crate::tabs::switch_tab(
                                    &app, &store, &adblock, &ts, &gid, idx,
                                );
                            }
                        })
                        .ok();
                }
            }
        }
    }

    fn chrome_thread_main(
        app: AppHandle,
        tx: mpsc::Sender<ChromeCmd>,
        rx: mpsc::Receiver<ChromeCmd>,
        wake: HANDLE,
    ) {
        unsafe { register_class() };
        let app_log = app.clone();
        CHROME.with(|c| {
            *c.borrow_mut() = Some(Chrome {
                app,
                tx,
                wake,
                hook: HHOOK::default(),
                captions: HashMap::new(),
                by_hwnd: HashMap::new(),
            });
        });

        let handles = [wake];
        loop {
            let waited = unsafe {
                MsgWaitForMultipleObjectsEx(
                    Some(&handles),
                    INFINITE,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                )
            };
            if waited == WAIT_FAILED {
                caption_log(&app_log, "message wait failed; chrome thread exiting");
                break;
            }
            while let Ok(cmd) = rx.try_recv() {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    unsafe { handle_chrome_cmd(cmd) }
                }));
                if r.is_err() {
                    caption_log(&app_log, "chrome command panicked; thread continues");
                }
            }
            // Pump messages: caption paint/mouse traffic and the hook proc
            // both run on this thread.
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
    }

    fn ensure_chrome(app: &AppHandle) -> Option<(mpsc::Sender<ChromeCmd>, Wake)> {
        let app_c = app.clone();
        Some(
            CHROME_CTL
                .get_or_init(move || {
                    let (tx, rx) = mpsc::channel::<ChromeCmd>();
                    let wake = unsafe { CreateEventW(None, false, false, None) }
                        .unwrap_or_else(|_| HANDLE::default());
                    let tx_thread = tx.clone();
                    let wake_thread = Wake(wake);
                    let wake_bits = wake.0 as isize;
                    let _ = std::thread::Builder::new()
                        .name("appmaka-caption".to_string())
                        .spawn(move || {
                            chrome_thread_main(
                                app_c,
                                tx_thread,
                                rx,
                                HANDLE(wake_bits as *mut _),
                            )
                        });
                    (tx, wake_thread)
                })
                .clone(),
        )
    }

    /// A page window (account or search) was created: give it a caption
    /// strip. Idempotent per label. Safe to call from any thread.
    ///
    /// Infallible by contract (v0.9.7): any failure — including a panic —
    /// leaves a plain frameless window (minimizable from the taskbar,
    /// closable via Alt+F4). A dead strip never kills the process.
    pub fn page_window_opened(app: &AppHandle, label: &str, window: &WebviewWindow) {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            page_window_opened_inner(app, label, window)
        }));
        if r.is_err() {
            caption_log(
                app,
                &format!(
                    "page_window_opened panicked for '{label}'; window continues without a strip"
                ),
            );
        }
    }

    fn page_window_opened_inner(app: &AppHandle, label: &str, window: &WebviewWindow) {
        let Ok(hwnd) = window.hwnd() else { return };
        let scale = window.scale_factor().unwrap_or(1.0);
        // The window title ("App — Account") becomes the strip's title.
        // Empty titles fall back to no text (the pre-v0.12.0 look).
        let title = window.title().unwrap_or_default();
        let Some((tx, wake)) = ensure_chrome(app) else {
            return;
        };
        let _ = tx.send(ChromeCmd::AddCaption {
            owner: hwnd.0 as isize,
            label: label.to_string(),
            scale,
            title,
        });
        unsafe {
            let _ = SetEvent(wake.0);
        }
    }

    /// The page window moved or resized: re-seat its strip above it.
    pub fn page_window_moved(label: &str) {
        if let Some((tx, wake)) = CHROME_CTL.get() {
            let _ = tx.send(ChromeCmd::Reposition {
                label: label.to_string(),
            });
            unsafe {
                let _ = SetEvent(wake.0);
            }
        }
    }

    /// The page window is gone: destroy its strip (no-op if Windows already
    /// took the owned window down with its owner).
    pub fn page_window_closed(label: &str) {
        if let Some((tx, wake)) = CHROME_CTL.get() {
            let _ = tx.send(ChromeCmd::RemoveCaption {
                label: label.to_string(),
            });
            unsafe {
                let _ = SetEvent(wake.0);
            }
        }
    }

    // ------------------------------------------------------------------
    // v0.10.0: tabbed-window strip API
    // ------------------------------------------------------------------

    /// Attach a tab strip to a tabbed page window. Same infallible
    /// contract as page_window_opened: any failure leaves a plain
    /// frameless window with working tabs via the dashboard.
    pub fn tabbed_window_opened(
        app: &AppHandle,
        label: &str,
        window: &WebviewWindow,
        tabs: TabStripData,
    ) {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Ok(hwnd) = window.hwnd() else { return };
            let scale = window.scale_factor().unwrap_or(1.0);
            let Some((tx, wake)) = ensure_chrome(app) else {
                return;
            };
            let _ = tx.send(ChromeCmd::AddTabbedCaption {
                owner: hwnd.0 as isize,
                label: label.to_string(),
                scale,
                tabs,
            });
            unsafe {
                let _ = SetEvent(wake.0);
            }
        }));
        if r.is_err() {
            caption_log(
                app,
                &format!(
                    "tabbed_window_opened panicked for '{label}'; window continues without tabs"
                ),
            );
        }
    }

    /// Refresh the tab strip's tabs (added/closed/switched) without
    /// rebuilding the window.
    pub fn tabbed_window_updated(label: &str, tabs: TabStripData) {
        if let Some((tx, wake)) = CHROME_CTL.get() {
            let _ = tx.send(ChromeCmd::UpdateTabbedTabs {
                label: label.to_string(),
                tabs,
            });
            unsafe {
                let _ = SetEvent(wake.0);
            }
        }
    }

    /// Re-tint the strip with the active tab's theme-color (None clears
    /// back to the default dark).
    pub fn tabbed_window_set_tint(label: &str, tint: Option<(u8, u8, u8)>) {
        if let Some((tx, wake)) = CHROME_CTL.get() {
            let _ = tx.send(ChromeCmd::SetTabTint {
                label: label.to_string(),
                tint,
            });
            unsafe {
                let _ = SetEvent(wake.0);
            }
        }
    }

    /// v0.11.0: tint a plain page window's strip with its resolved site
    /// color (None clears back to the dark default). No-op for tabbed
    /// windows — those own their tint via `tabbed_window_set_tint`.
    pub fn page_window_set_tint(label: &str, tint: Option<(u8, u8, u8)>) {
        if let Some((tx, wake)) = CHROME_CTL.get() {
            let _ = tx.send(ChromeCmd::SetPageTint {
                label: label.to_string(),
                tint,
            });
            unsafe {
                let _ = SetEvent(wake.0);
            }
        }
    }

    /// v0.11.0: tint-decision log for `tint.rs`. Same file and rotation
    /// policy as `caption_log` so one log tells the whole top-bar story.
    pub fn tint_log(app: &AppHandle, msg: &str) {
        caption_log(app, msg);
    }
}

#[cfg(windows)]
pub use imp::{
    page_window_closed, page_window_moved, page_window_opened, page_window_set_tint,
    tabbed_window_opened, tabbed_window_set_tint, tabbed_window_updated, tint_log, TabStripData,
};

#[cfg(not(windows))]
pub fn page_window_opened(
    _app: &tauri::AppHandle,
    _label: &str,
    _window: &tauri::WebviewWindow,
) {
    // Linux keeps native decorations: no caption strips, no gesture hook.
}

#[cfg(not(windows))]
pub fn page_window_moved(_label: &str) {}

#[cfg(not(windows))]
pub fn page_window_closed(_label: &str) {}

/// v0.10.0 non-Windows stubs: the tab strip is an HTML window on Linux
/// (see tabs.rs); these are no-ops elsewhere.
#[cfg(not(windows))]
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TabStripData {
    pub tabs: Vec<String>,
    pub active: usize,
    pub tint: Option<(u8, u8, u8)>,
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn tabbed_window_opened(
    _app: &tauri::AppHandle,
    _label: &str,
    _window: &tauri::WebviewWindow,
    _tabs: TabStripData,
) {
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn tabbed_window_updated(_label: &str, _tabs: TabStripData) {}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn tabbed_window_set_tint(_label: &str, _tint: Option<(u8, u8, u8)>) {}

/// v0.11.0 non-Windows stubs: no caption strips on Linux.
#[cfg(not(windows))]
#[allow(dead_code)]
pub fn page_window_set_tint(_label: &str, _tint: Option<(u8, u8, u8)>) {}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn tint_log(_app: &tauri::AppHandle, _msg: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn gesture(esc_down: bool, lmb_held: bool, foreground_is_page: bool) -> EscGesture {
        EscGesture {
            esc_down,
            lmb_held,
            foreground_is_page,
        }
    }

    #[test]
    fn esc_plus_held_lmb_on_page_closes() {
        assert!(esc_gesture_closes(gesture(true, true, true)));
    }

    #[test]
    fn esc_alone_never_closes() {
        // Esc without the held button: page keeps its own Esc handling
        // (dialogs, menus, blur).
        assert!(!esc_gesture_closes(gesture(true, false, true)));
    }

    #[test]
    fn lmb_held_but_foreground_is_not_a_page_never_closes() {
        // The launcher, Settings, and the clipboard popup keep their Esc
        // behaviors: holding LMB there and pressing Esc does nothing new.
        assert!(!esc_gesture_closes(gesture(true, true, false)));
    }

    #[test]
    fn no_esc_event_never_closes() {
        // Other keys (or key-up) with LMB held: untouched.
        assert!(!esc_gesture_closes(gesture(false, true, true)));
        assert!(!esc_gesture_closes(gesture(false, false, false)));
    }

    #[test]
    fn minimized_window_cannot_match() {
        // A minimized window can never be the foreground window, so the
        // classifier's foreground gate already excludes it: it arrives here
        // as foreground_is_page = false and passes through.
        assert!(!esc_gesture_closes(gesture(true, true, false)));
    }

    #[test]
    fn fullscreen_page_still_closes() {
        // Fullscreen keeps the page in the foreground, so the gesture
        // applies there too — documented, not special-cased.
        assert!(esc_gesture_closes(gesture(true, true, true)));
    }

    #[test]
    fn every_other_combination_passes_through() {
        // The classifier is a three-input AND: exhaust the remaining
        // non-closing combos so a future edit can't silently widen it.
        assert!(!esc_gesture_closes(gesture(true, false, false)));
        assert!(!esc_gesture_closes(gesture(false, true, false)));
        assert!(!esc_gesture_closes(gesture(false, false, true)));
    }
}
