//! Text budgets shared by both directions.
//!
//! Byte caps count UTF-8 bytes, never characters, because ntfy and the phone
//! client count bytes. Truncation never splits a character.

/// Relay ack cap, `ACK_MAX_BYTES` in the Node relay.
pub const ACK_MAX_BYTES: usize = 3400;

/// ECMAScript `\s`. Rust's `char::is_whitespace` differs at U+0085 and U+FEFF.
pub fn is_js_space(ch: char) -> bool {
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

/// `text.trim()` with JavaScript whitespace.
pub fn trim_js(text: &str) -> &str {
    text.trim_matches(is_js_space)
}

/// `clampBytes`: keep whole characters under `limit - 3` bytes, then append
/// `...`. Text that already fits is returned unchanged; a limit below three
/// bytes leaves no room for the marker and returns an empty string.
pub fn clamp_bytes(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    if limit < 3 {
        return String::new();
    }
    let budget = limit - 3;
    let mut used = 0;
    let mut end = text.len();
    for (index, ch) in text.char_indices() {
        if used + ch.len_utf8() > budget {
            end = index;
            break;
        }
        used += ch.len_utf8();
    }
    let mut out = String::with_capacity(used + 3);
    out.push_str(&text[..end]);
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_whole_characters_under_the_cap() {
        let ascii = clamp_bytes(&"x".repeat(10_000), ACK_MAX_BYTES);
        assert_eq!(ascii.len(), ACK_MAX_BYTES);
        assert!(ascii.ends_with("..."));

        let multibyte = clamp_bytes(&"\u{e9}".repeat(3000), ACK_MAX_BYTES);
        assert!(multibyte.len() <= ACK_MAX_BYTES);
        assert!(multibyte.ends_with("..."));

        assert_eq!(clamp_bytes("short", ACK_MAX_BYTES), "short");
    }

    #[test]
    fn clamp_never_exceeds_a_small_limit() {
        assert_eq!(clamp_bytes("abc", 2), "");
        assert_eq!(clamp_bytes("ab", 2), "ab");
        assert_eq!(clamp_bytes("abcd", 3), "...");
        assert_eq!(clamp_bytes("abcd", 4), "abcd");
        assert_eq!(clamp_bytes("abcde", 4), "a...");
        assert_eq!(clamp_bytes("abcd", 0), "");
        assert_eq!(clamp_bytes("", 0), "");
    }

    #[test]
    fn javascript_whitespace_matches_the_trim_cases() {
        assert!(trim_js("\u{feff}\t hi \n\u{00a0}").starts_with("hi"));
        assert!(!is_js_space('\u{0085}'));
        assert!(is_js_space('\u{3000}'));
    }
}
