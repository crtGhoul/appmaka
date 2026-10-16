//! Unread-count badges from live page titles.
//!
//! Sites announce unread state in `document.title`: Gmail's "(3) Inbox",
//! Discord's "(2) Discord", WhatsApp's "(5) WhatsApp". The launcher shows
//! that count on the account's tile and Library row — but only when the
//! title matches a shape we trust, because a wrong badge is worse than
//! no badge. Trusted shapes:
//!
//! - a leading `(N)` or `(N+)`, e.g. "(3) Inbox - Gmail"
//! - a trailing `• N` or `| N`, e.g. "Inbox • 3"
//!
//! A four-digit number in the year range is a year, not a count
//! ("(2026) Report.pdf" must never badge), and anything that isn't
//! cleanly a small number returns `None`.

/// Extract an unread count from a page title, or `None` when the title
/// doesn't carry one we trust. Pure — unit tests pin the corpus.
pub fn parse_unread_count(title: &str) -> Option<u32> {
    let t = title.trim();
    if t.is_empty() {
        return None;
    }
    leading_count(t).or_else(|| trailing_count(t))
}

/// `(N)` or `(N+)` at the very start of the title.
fn leading_count(t: &str) -> Option<u32> {
    let rest = t.strip_prefix('(')?;
    let end = rest.find(')')?;
    let digits = rest[..end].trim_end_matches('+');
    let n: u32 = digits.parse().ok()?;
    valid_count(n, digits)
}

/// `• N` or `| N` at the very end of the title.
fn trailing_count(t: &str) -> Option<u32> {
    let sep = t.rfind(['•', '|'])?;
    let sep_char = t.get(sep..)?.chars().next()?;
    let digits = t
        .get(sep + sep_char.len_utf8()..)?
        .trim()
        .trim_end_matches('+');
    let n: u32 = digits.parse().ok()?;
    valid_count(n, digits)
}

/// A count is only a count when it's small, nonzero, and not a year.
fn valid_count(n: u32, digits: &str) -> Option<u32> {
    if n == 0 || n > 9999 {
        return None;
    }
    if digits.len() == 4 && (1900..=2100).contains(&n) {
        return None;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_world_titles() {
        assert_eq!(parse_unread_count("(3) Inbox - user@gmail.com - Gmail"), Some(3));
        assert_eq!(parse_unread_count("(99+) Inbox - Gmail"), Some(99));
        assert_eq!(parse_unread_count("(1) Discord"), Some(1));
        assert_eq!(parse_unread_count("(5) WhatsApp"), Some(5));
        assert_eq!(parse_unread_count("(12+) Discord"), Some(12));
        assert_eq!(parse_unread_count("Inbox • 3"), Some(3));
        assert_eq!(parse_unread_count("Discord | 12"), Some(12));
        assert_eq!(parse_unread_count("• 7"), Some(7));
    }

    #[test]
    fn no_count_titles_stay_quiet() {
        assert_eq!(parse_unread_count("YouTube Music"), None);
        assert_eq!(parse_unread_count("reddit: the heart of the internet"), None);
        assert_eq!(parse_unread_count("Gmail — Default"), None);
        assert_eq!(parse_unread_count(""), None);
        assert_eq!(parse_unread_count("   "), None);
        assert_eq!(parse_unread_count("(0) Inbox"), None);
        assert_eq!(parse_unread_count("(+) Inbox"), None);
    }

    #[test]
    fn years_and_noise_never_badge() {
        // The false positive that motivated the year guard.
        assert_eq!(parse_unread_count("(2026) Report.pdf"), None);
        assert_eq!(parse_unread_count("Meeting notes | 2026"), None);
        assert_eq!(parse_unread_count("(10000) Inbox"), None);
        // Conservative: comma-grouped thousands are not parsed.
        assert_eq!(parse_unread_count("(1,234) Discord"), None);
        // The number must be the whole trailing segment.
        assert_eq!(parse_unread_count("Chat | 3 new messages"), None);
        assert_eq!(parse_unread_count("Artist • Topic"), None);
    }
}
