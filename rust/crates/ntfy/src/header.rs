//! Header encoding and text budgets shared by the three publish styles.
//!
//! Mirrors `headerText`, `normalizeTags`, `sanitizeTitle`, and `truncateUtf8`
//! in the Node relay and plugin clients.

use std::fmt;
use std::str::FromStr;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

/// Plugin cap for one title, in UTF-8 bytes (`MAX_TITLE_BYTES`).
pub const MAX_TITLE_BYTES: usize = 1024;
/// Plugin cap for one message, in UTF-8 bytes (`MAX_MESSAGE_BYTES`).
pub const MAX_MESSAGE_BYTES: usize = 4096;

/// ntfy priority names in wire order. The JSON API takes the numeric level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Min,
    Low,
    Default,
    High,
    Max,
}

impl Priority {
    pub fn as_name(self) -> &'static str {
        match self {
            Self::Min => "min",
            Self::Low => "low",
            Self::Default => "default",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    /// Numeric level used by the JSON body and the PUT query string.
    pub fn as_level(self) -> u8 {
        match self {
            Self::Min => 1,
            Self::Low => 2,
            Self::Default => 3,
            Self::High => 4,
            Self::Max => 5,
        }
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_name())
    }
}

impl FromStr for Priority {
    type Err = PriorityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "min" => Ok(Self::Min),
            "low" => Ok(Self::Low),
            "default" => Ok(Self::Default),
            "high" => Ok(Self::High),
            "max" => Ok(Self::Max),
            _ => Err(PriorityError),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorityError;

impl fmt::Display for PriorityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("priority must be min, low, default, high, or max")
    }
}

impl std::error::Error for PriorityError {}

/// Returns at most `max_bytes` of `text` on a UTF-8 character boundary.
///
/// Same walk as the plugin's `truncateUtf8`: cut at the byte limit, then step
/// back over continuation bytes. No marker is appended.
pub fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Replaces C0, DEL, and C1 controls with a space, then trims like JavaScript.
pub fn sanitize_title(text: &str) -> String {
    let replaced: String = text
        .chars()
        .map(|c| if is_control(c) { ' ' } else { c })
        .collect();
    trim_js(&replaced).to_string()
}

/// Builds one HTTP header value: newlines collapse to spaces, empty becomes
/// `-`, and non-ASCII is RFC 2047 base64. `headerText` in the relay.
pub fn header_text(value: &str) -> String {
    let mut collapsed = String::with_capacity(value.len());
    let mut in_break = false;
    for c in value.chars() {
        if c == '\r' || c == '\n' {
            if !in_break {
                collapsed.push(' ');
                in_break = true;
            }
        } else {
            collapsed.push(c);
            in_break = false;
        }
    }
    let text = trim_js(&collapsed);
    if text.is_empty() {
        return "-".to_string();
    }
    if text.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return text.to_string();
    }
    format!("=?UTF-8?B?{}?=", STANDARD.encode(text.as_bytes()))
}

/// Drops tags that would break the comma-separated header. Order is kept.
pub fn normalize_tags<'a>(tags: &[&'a str]) -> Vec<&'a str> {
    tags.iter()
        .copied()
        .filter(|tag| {
            !tag.is_empty()
                && tag
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .collect()
}

fn is_control(c: char) -> bool {
    c <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&c)
}

/// JavaScript `String.prototype.trim` set: U+0085 is not whitespace here.
fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_space)
}

fn is_js_space(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\u{09}'..='\u{0d}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_text_matches_relay_cases() {
        assert_eq!(header_text("plain"), "plain");
        assert_eq!(header_text("two\nlines"), "two lines");
        assert_eq!(header_text("a\r\nb\nc"), "a b c");
        assert_eq!(header_text(""), "-");
        assert_eq!(header_text("Caf\u{e9}"), "=?UTF-8?B?Q2Fmw6k=?=");
    }

    #[test]
    fn truncate_keeps_a_character_boundary() {
        assert_eq!(truncate_utf8("abc", 10), "abc");
        assert_eq!(truncate_utf8("a\u{e9}", 2), "a");
        assert_eq!(truncate_utf8("a\u{e9}", 3), "a\u{e9}");
        assert_eq!(truncate_utf8("abc", 0), "");
    }

    #[test]
    fn sanitize_title_replaces_controls_and_trims() {
        assert_eq!(sanitize_title("  hi\tthere\u{7f}  "), "hi there");
        assert_eq!(sanitize_title("\u{feff}x\u{feff}"), "x");
    }

    #[test]
    fn header_trim_keeps_u0085() {
        // U+0085 is not JavaScript whitespace; it is encoded, not trimmed.
        let encoded = header_text("hi\u{85}");
        assert!(encoded.starts_with("=?UTF-8?B?"));
        assert_ne!(encoded, header_text("hi"));
        assert_eq!(sanitize_title("x\u{85}"), "x");
    }

    #[test]
    fn tags_filter_and_keep_order() {
        assert_eq!(
            normalize_tags(&["oc-out", "bad tag", "ok_1", "", "a,b"]),
            vec!["oc-out", "ok_1"]
        );
    }

    #[test]
    fn priority_maps_both_wire_forms() {
        assert_eq!(Priority::Min.as_name(), "min");
        assert_eq!(Priority::Min.as_level(), 1);
        assert_eq!(Priority::Max.as_level(), 5);
        assert_eq!("high".parse::<Priority>(), Ok(Priority::High));
        assert!("urgent".parse::<Priority>().is_err());
    }
}
