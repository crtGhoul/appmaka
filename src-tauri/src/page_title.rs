//! Page-title fetch for the quick-add flow.
//!
//! The user pastes a URL; this does a plain HTTP GET (browser user-agent,
//! 10 s timeout, body capped at 512 KiB since the title is always near the
//! top) and extracts `<title>`. Every failure is a plain-string error the
//! frontend can show; the UI falls back to a prettified domain name, so this
//! is best-effort by design. It never touches credentials or sessions.

use std::io::Read;
use std::time::Duration;

const MAX_BODY: u64 = 512 * 1024;
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36";

/// Fetch the page title for `url`. Errors are plain strings for the UI.
pub fn fetch_page_title(url: &str) -> Result<String, String> {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("URL must start with http:// or https://.".to_string());
    }
    let mut body = Vec::new();
    ureq::get(url)
        .set("User-Agent", UA)
        .timeout(Duration::from_secs(10))
        .call()
        .map_err(|e| format!("Could not load the page: {e}"))?
        .into_reader()
        .take(MAX_BODY)
        .read_to_end(&mut body)
        .map_err(|e| format!("Could not read the page: {e}"))?;
    let html = String::from_utf8_lossy(&body);
    extract_title(&html).ok_or_else(|| "No page title found.".to_string())
}

/// Case-insensitive `<title>…</title>` extraction: whitespace collapsed and
/// the common HTML entities decoded. Returns None when there is no usable
/// title.
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let open = lower.find("<title")?;
    let content_start = open + lower[open..].find('>')? + 1;
    let content_end = content_start + lower[content_start..].find("</title>")?;
    let raw = html.get(content_start..content_end)?.trim();
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(
        collapsed
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&nbsp;", " "),
    )
}

/// Best-effort `theme-color` meta tag value, normalized to lowercase
/// `#rrggbb`. Same fetch as `fetch_page_title` (browser UA, 10 s timeout,
/// 512 KiB cap); returns None on any failure or when the tag is absent or
/// unparseable. Used for the popup title-bar tint (v0.9.11) — never a
/// hard requirement, the native bar stays when this is None.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn fetch_theme_color(url: &str) -> Option<String> {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let mut body = Vec::new();
    ureq::get(url)
        .set("User-Agent", UA)
        .timeout(Duration::from_secs(10))
        .call()
        .ok()?
        .into_reader()
        .take(MAX_BODY)
        .read_to_end(&mut body)
        .ok()?;
    let html = String::from_utf8_lossy(&body);
    extract_theme_color(&html)
}

/// Case-insensitive `<meta name="theme-color" content="…">` extraction.
/// Accepts `#rgb` and `#rrggbb`; anything else (named colors, `rgb()`,
/// CSS variables) is out of scope for a title-bar tint.
#[cfg_attr(not(windows), allow(dead_code))]
fn extract_theme_color(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let mut search_from = 0;
    while let Some(open) = lower[search_from..].find("<meta") {
        let tag_start = search_from + open;
        let tag_end = tag_start + lower[tag_start..].find('>')?;
        let tag = &lower[tag_start..tag_end];
        if tag.contains("name=\"theme-color\"") || tag.contains("name='theme-color'") {
            if let Some(color) = attr_value(tag, "content") {
                if let Some(hex) = normalize_theme_hex(&color) {
                    return Some(hex);
                }
            }
        }
        search_from = tag_end + 1;
    }
    None
}

/// Value of `attr="…"` (or `attr='…'`) inside one lowercased tag.
#[cfg_attr(not(windows), allow(dead_code))]
fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=");
    let start = tag.find(&needle)? + needle.len();
    let rest = tag[start..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let end = rest[1..].find(quote)?;
    Some(rest[1..1 + end].to_string())
}

/// `#rgb` → `#rrggbb`, `#rrggbb` → lowercase; else None.
#[cfg_attr(not(windows), allow(dead_code))]
fn normalize_theme_hex(raw: &str) -> Option<String> {
    let s = raw.trim().strip_prefix('#')?;
    let expanded = match s.len() {
        3 => s.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => s.to_string(),
        _ => return None,
    };
    if !expanded.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{}", expanded.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::{extract_theme_color, extract_title};

    #[test]
    fn extracts_simple_title() {
        let html = "<html><head><title>  Hello&nbsp;World </title></head></html>";
        assert_eq!(extract_title(html).as_deref(), Some("Hello World"));
    }

    #[test]
    fn case_insensitive_with_entities_and_attrs() {
        let html = "<HTML><HEAD><TITLE class=\"x\">Fish &amp; Chips</TITLE></HEAD>";
        assert_eq!(extract_title(html).as_deref(), Some("Fish & Chips"));
    }

    #[test]
    fn missing_title_is_none() {
        assert_eq!(extract_title("<html><body>nope</body></html>"), None);
    }

    #[test]
    fn theme_color_double_quotes() {
        let html = r##"<head><meta name="theme-color" content="#1a2B3c"></head>"##;
        assert_eq!(extract_theme_color(html).as_deref(), Some("#1a2b3c"));
    }

    #[test]
    fn theme_color_short_hex_expands() {
        let html = r##"<head><meta name='theme-color' content='#abc'></head>"##;
        assert_eq!(extract_theme_color(html).as_deref(), Some("#aabbcc"));
    }

    #[test]
    fn theme_color_ignores_other_metas_and_bad_values() {
        let html = r##"<head><meta name="viewport" content="width=1"><meta name="theme-color" content="red"></head>"##;
        assert_eq!(extract_theme_color(html), None);
        let html2 = r##"<head><meta name="theme-color" content="#12"></head>"##;
        assert_eq!(extract_theme_color(html2), None);
        let html3 = "<head></head>";
        assert_eq!(extract_theme_color(html3), None);
    }

    #[test]
    fn theme_color_case_insensitive_tag() {
        let html = r##"<HEAD><META NAME="THEME-COLOR" CONTENT="#FFF"></HEAD>"##;
        assert_eq!(extract_theme_color(html).as_deref(), Some("#ffffff"));
    }
}
