//! Universal title-bar blending (v0.11.0).
//!
//! Every AppMaka window with a tintable top bar resolves its color through
//! one fallback chain:
//!
//! 1. `theme-color` meta tag via HTTP fetch (fast path — covers static
//!    pages; the existing `page_title::fetch_theme_color`).
//! 2. Live DOM via a webview JS eval (only when 1 misses): the page's own
//!    `theme-color` meta first (catches JS-set values the static fetch
//!    can't see), then the computed page background walking up from
//!    `<body>` past transparent.
//! 3. Default: no tint (DWM reset / strip repainted dark).
//!
//! Application differs per window type on Windows:
//! - popups keep native decorations → `DwmSetWindowAttribute(DWMWA_CAPTION_COLOR)`.
//! - page windows are frameless with a caption.rs strip → `SetPageTint` repaint.
//! - tabbed windows → the existing `SetTabTint` (active tab's color).
//! The launcher keeps its own look (out of scope). On Linux there is no
//! stable API to tint native decorations; the tabbed HTML strip already
//! tints itself — documented platform limit, not attempted here.
//!
//! Every decision is logged to `caption-errors.log` (window, target, source,
//! final color) so a future log proves what happened on real hardware —
//! the DWM path was logic-only until this release.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tauri::{AppHandle, Manager};

/// Where a tint color came from. Logged per decision.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TintSource {
    /// `<meta name="theme-color">` from the static HTTP fetch.
    ThemeMetaHttp,
    /// `theme-color` meta found in the live DOM (JS-set).
    ThemeMetaDom,
    /// Computed page background color via JS.
    PageBackground,
    /// No color found — default top bar.
    Default,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl TintSource {
    fn log_name(self) -> &'static str {
        match self {
            TintSource::ThemeMetaHttp => "theme-meta-http",
            TintSource::ThemeMetaDom => "theme-meta-dom",
            TintSource::PageBackground => "page-background",
            TintSource::Default => "default",
        }
    }
}

/// Which top bar to tint.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TintTarget {
    /// Native title bar (popups): DWM caption color.
    NativeCaption,
    /// Frameless custom strip (page windows): caption.rs repaint.
    PageStrip,
    /// Tabbed strip: caption.rs repaint with the active tab's color.
    TabStrip,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl TintTarget {
    fn log_name(self) -> &'static str {
        match self {
            TintTarget::NativeCaption => "dwm",
            TintTarget::PageStrip => "page-strip",
            TintTarget::TabStrip => "tab-strip",
        }
    }
}

/// A resolved tint. `rgb: None` means "default top bar" (source Default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TintDecision {
    pub rgb: Option<(u8, u8, u8)>,
    pub source: TintSource,
}

/// Parse a CSS color into `(r, g, b)`. Accepts `#rgb`, `#rrggbb`,
/// `rgb(r, g, b)` and `rgba(r, g, b, a)` (alpha 0 = transparent → None).
/// Named colors and anything else are out of scope for a title-bar tint.
pub fn parse_css_color(raw: &str) -> Option<(u8, u8, u8)> {
    let s = raw.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let expanded = match hex.len() {
            3 => hex.chars().flat_map(|c| [c, c]).collect::<String>(),
            6 => hex.to_string(),
            _ => return None,
        };
        if !expanded.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(&expanded, 16).ok()?;
        return Some(((v >> 16) as u8, (v >> 8) as u8, v as u8));
    }
    let lower = s.to_lowercase();
    let inner = lower
        .strip_prefix("rgba(")
        .or_else(|| lower.strip_prefix("rgb("))?;
    let inner = inner.strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() != 3 && parts.len() != 4 {
        return None;
    }
    let r: u8 = parts[0].parse().ok()?;
    let g: u8 = parts[1].parse().ok()?;
    let b: u8 = parts[2].parse().ok()?;
    if parts.len() == 4 {
        let a: f32 = parts[3].parse().ok()?;
        if a <= 0.0 {
            return None;
        }
    }
    Some((r, g, b))
}

/// Parse the DOM probe's return value: `meta:<css>`, `bg:<css>`, or `none`.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_dom_result(raw: &str) -> Option<(TintSource, (u8, u8, u8))> {
    let s = raw.trim();
    if s == "none" {
        return None;
    }
    if let Some(css) = s.strip_prefix("meta:") {
        return parse_css_color(css).map(|rgb| (TintSource::ThemeMetaDom, rgb));
    }
    if let Some(css) = s.strip_prefix("bg:") {
        return parse_css_color(css).map(|rgb| (TintSource::PageBackground, rgb));
    }
    None
}

/// JS probe: live `theme-color` meta first, else the computed background
/// walking up from `<body>` past transparent. Always returns a plain
/// string (`meta:…`, `bg:…`, or `none`) — never throws (Windows swallows
/// eval exceptions, so the try/catch is load-bearing).
#[cfg_attr(not(windows), allow(dead_code))]
fn dom_probe_js() -> &'static str {
    r#"(() => {
  try {
    var m = document.querySelector('meta[name="theme-color"]');
    var mc = m && m.getAttribute('content');
    if (mc && mc.trim()) return 'meta:' + mc.trim();
    var clear = /^\s*rgba?\(\s*0\s*,\s*0\s*,\s*0\s*(,\s*0(\.0*)?\s*)?\)\s*$/;
    var el = document.body, bg;
    while (el) {
      bg = getComputedStyle(el).backgroundColor;
      if (bg && bg !== 'transparent' && !clear.test(bg)) return 'bg:' + bg;
      el = el.parentElement;
    }
    bg = getComputedStyle(document.documentElement).backgroundColor;
    if (bg && bg !== 'transparent' && !clear.test(bg)) return 'bg:' + bg;
    return 'none';
  } catch (e) { return 'none'; }
})()"#
}

/// Fast path only: static HTTP `theme-color` fetch, no DOM. Used by the
/// async chain below and by callers that need a synchronous answer
/// (e.g. tabs.rs group-state bookkeeping).
pub fn resolve_fast(url: &str) -> TintDecision {
    let rgb = crate::page_title::fetch_theme_color(url).and_then(|h| parse_css_color(&h));
    match rgb {
        Some(rgb) => TintDecision {
            rgb: Some(rgb),
            source: TintSource::ThemeMetaHttp,
        },
        None => TintDecision {
            rgb: None,
            source: TintSource::Default,
        },
    }
}

/// Ask the live page for its color via the DOM probe. `None` when the
/// window is gone, the eval fails/times out, or the page reports nothing.
#[cfg_attr(not(windows), allow(dead_code))]
fn eval_dom_color(app: &AppHandle, label: &str) -> Option<(TintSource, (u8, u8, u8))> {
    let window = app.get_webview_window(label)?;
    let (tx, rx) = std::sync::mpsc::channel();
    window
        .eval_with_callback(dom_probe_js(), move |json| {
            let _ = tx.send(json);
        })
        .ok()?;
    let json = rx.recv_timeout(Duration::from_secs(5)).ok()?;
    // eval_with_callback JSON-serializes the result: a returned string
    // arrives wrapped in double quotes.
    let s = json.trim();
    let s = s
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s);
    if s.contains('\\') {
        return None;
    }
    parse_dom_result(s)
}

/// Full synchronous resolution: fast HTTP path, then the DOM probe after a
/// short settle delay (on_navigation fires at navigation *start*, before
/// any DOM exists), one retry, then default. Cross-platform so the Xvfb
/// E2E can exercise the decision logic headless.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn resolve_now(app: &AppHandle, label: &str, url: &str) -> TintDecision {
    let fast = resolve_fast(url);
    if fast.rgb.is_some() {
        return fast;
    }
    std::thread::sleep(Duration::from_millis(1000));
    if let Some((source, rgb)) = eval_dom_color(app, label) {
        return TintDecision {
            rgb: Some(rgb),
            source,
        };
    }
    std::thread::sleep(Duration::from_millis(2000));
    match eval_dom_color(app, label) {
        Some((source, rgb)) => TintDecision {
            rgb: Some(rgb),
            source,
        },
        None => TintDecision {
            rgb: None,
            source: TintSource::Default,
        },
    }
}

// ---------------------------------------------------------------------------
// Async entry point (Windows): dedup + generation guard + detached thread.
// ---------------------------------------------------------------------------

#[cfg_attr(not(windows), allow(dead_code))]
fn generations() -> &'static Mutex<HashMap<String, u64>> {
    static G: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn completed() -> &'static Mutex<HashMap<String, String>> {
    static C: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn next_generation(label: &str) -> u64 {
    match generations().lock() {
        Ok(mut g) => {
            let n = g.get(label).copied().unwrap_or(0) + 1;
            g.insert(label.to_string(), n);
            n
        }
        Err(_) => 0,
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn generation_current(label: &str, gen: u64) -> bool {
    generations()
        .lock()
        .map(|g| g.get(label).copied().unwrap_or(0) == gen)
        .unwrap_or(false)
}

/// Request a (re-)tint for a window. Best-effort: any failure leaves the
/// current top bar untouched, and every decision is logged. Never blocks
/// the caller — the HTTP fetch and DOM probe run on a detached thread.
#[cfg(windows)]
pub fn request_retint(app: &AppHandle, label: &str, url: &url::Url, target: TintTarget) {
    let url_str = url.to_string();
    let gen = next_generation(label);
    if !(url_str.starts_with("http://") || url_str.starts_with("https://")) {
        let d = TintDecision {
            rgb: None,
            source: TintSource::Default,
        };
        apply_and_log(app, label, &url_str, target, d);
        return;
    }
    // Skip when this exact URL already resolved for this window — but only
    // when a previous resolution *completed*; an in-flight one must not
    // suppress a re-request (its generation is stale by now).
    let already = completed()
        .lock()
        .map(|c| c.get(label).is_some_and(|u| u == &url_str))
        .unwrap_or(false);
    if already {
        return;
    }
    let app = app.clone();
    let label = label.to_string();
    std::thread::Builder::new()
        .name(format!("appmaka-tint-{label}"))
        .spawn(move || {
            let d = resolve_now(&app, &label, &url_str);
            if generation_current(&label, gen) {
                apply_and_log(&app, &label, &url_str, target, d);
            }
        })
        .ok();
}

/// Non-Windows stub: no stable API tints native decorations on Linux.
#[cfg(not(windows))]
pub fn request_retint(_app: &AppHandle, _label: &str, _url: &url::Url, _target: TintTarget) {}

#[cfg(windows)]
fn apply_and_log(
    app: &AppHandle,
    label: &str,
    url: &str,
    target: TintTarget,
    d: TintDecision,
) {
    let color_hex = d
        .rgb
        .map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}"))
        .unwrap_or_else(|| "default".to_string());
    crate::caption::tint_log(
        app,
        &format!(
            "tint label={label} target={} url={url} source={} color={color_hex}",
            target.log_name(),
            d.source.log_name(),
        ),
    );
    if let Ok(mut c) = completed().lock() {
        c.insert(label.to_string(), url.to_string());
    }
    match target {
        TintTarget::NativeCaption => {
            if let Some(w) = app.get_webview_window(label) {
                if let Ok(hwnd) = w.hwnd() {
                    apply_dwm_caption(hwnd, d.rgb);
                }
            }
        }
        TintTarget::PageStrip => crate::caption::page_window_set_tint(label, d.rgb),
        TintTarget::TabStrip => crate::caption::tabbed_window_set_tint(label, d.rgb),
    }
}

/// `DWMWA_COLOR_DEFAULT` isn't in the `windows` crate; `0xFFFFFFFE` is the
/// community-verified reset value (restores the default caption color).
#[cfg(windows)]
fn apply_dwm_caption(
    hwnd: windows::Win32::Foundation::HWND,
    rgb: Option<(u8, u8, u8)>,
) {
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CAPTION_COLOR};
    const DWMWA_COLOR_DEFAULT: u32 = 0xFFFFFFFE;
    // COLORREF is 0x00BBGGRR.
    let color: u32 = match rgb {
        Some((r, g, b)) => r as u32 | ((g as u32) << 8) | ((b as u32) << 16),
        None => DWMWA_COLOR_DEFAULT,
    };
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &color as *const u32 as *const std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_css_color, parse_dom_result, TintSource};

    #[test]
    fn css_hex_forms() {
        assert_eq!(parse_css_color("#1a2b3c"), Some((0x1a, 0x2b, 0x3c)));
        assert_eq!(parse_css_color("#ABC"), Some((0xaa, 0xbb, 0xcc)));
        assert_eq!(parse_css_color("  #ffffff  "), Some((0xff, 0xff, 0xff)));
    }

    #[test]
    fn css_rgb_forms() {
        assert_eq!(parse_css_color("rgb(26, 43, 60)"), Some((26, 43, 60)));
        assert_eq!(parse_css_color("rgba(26,43,60,0.5)"), Some((26, 43, 60)));
        assert_eq!(parse_css_color("RGB(0,0,0)"), Some((0, 0, 0)));
    }

    #[test]
    fn css_rejects_transparent_and_garbage() {
        assert_eq!(parse_css_color("rgba(0, 0, 0, 0)"), None);
        assert_eq!(parse_css_color("transparent"), None);
        assert_eq!(parse_css_color("red"), None);
        assert_eq!(parse_css_color("#12"), None);
        assert_eq!(parse_css_color("#gggggg"), None);
        assert_eq!(parse_css_color("rgb(1,2)"), None);
        assert_eq!(parse_css_color(""), None);
    }

    #[test]
    fn dom_result_routes_source() {
        assert_eq!(
            parse_dom_result("meta:#1a2b3c"),
            Some((TintSource::ThemeMetaDom, (0x1a, 0x2b, 0x3c)))
        );
        assert_eq!(
            parse_dom_result("bg:rgb(10, 20, 30)"),
            Some((TintSource::PageBackground, (10, 20, 30)))
        );
        assert_eq!(parse_dom_result("none"), None);
        assert_eq!(parse_dom_result("meta:red"), None);
        assert_eq!(parse_dom_result("garbage"), None);
    }

    #[test]
    fn resolve_fast_prefers_http_meta() {
        // No network in unit tests: an invalid URL resolves to default.
        let d = super::resolve_fast("not a url");
        assert_eq!(d.rgb, None);
        assert_eq!(d.source, TintSource::Default);
    }
}
