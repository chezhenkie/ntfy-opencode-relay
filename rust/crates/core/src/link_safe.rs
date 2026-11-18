//! Link-safe text for ntfy pushes.
//!
//! The Android client linkifies any token that looks like a domain, so a bare
//! file name such as `readme.md` becomes a link to the `.md` country domain.
//! Tokens whose last label is on [`ALLOWED_TLDS`] stay untouched; every other
//! domain token gets its last dot replaced with `[.]` when the label is a real
//! IANA TLD or a two-letter code. Scheme URLs are skipped whole. Ported from
//! the Node relay `link-safe.mjs`; the fixture outputs come from that copy.

use crate::text::is_js_space;
use crate::tlds::is_iana_tld;

/// TLDs whose tokens stay linkable, matching `ALLOWED_TLDS` in Node. Sorted
/// for binary search.
pub const ALLOWED_TLDS: [&str; 68] = [
    "ae", "ai", "app", "at", "au", "be", "bg", "biz", "blog", "br", "ca", "ch", "cloud", "cn",
    "co", "com", "cz", "de", "dev", "dk", "edu", "ee", "es", "eu", "fi", "fr", "gov", "gr", "hk",
    "hr", "hu", "ie", "il", "in", "info", "int", "io", "is", "it", "jp", "kr", "lt", "lu", "lv",
    "mil", "mx", "net", "nl", "no", "nz", "online", "org", "pl", "pt", "ro", "se", "sg", "shop",
    "si", "site", "sk", "store", "tech", "tw", "ua", "uk", "us", "za",
];

/// Membership in the sorted allowlist.
pub fn is_allowed_tld(tld: &str) -> bool {
    ALLOWED_TLDS.binary_search(&tld).is_ok()
}

/// Returns text with every non-allowed domain-like token defanged. Two-letter
/// suffixes are always treated as domains, so an unknown country domain cannot
/// slip through. Scheme URLs, numbers, paths and ordinary dotted words pass.
pub fn link_safe(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut index = 0;
    while index < text.len() {
        if let Some(end) = match_scheme(text, index) {
            index = end;
            continue;
        }
        if let Some(end) = match_word(text, index) {
            out.push_str(&text[copied..index]);
            let token = &text[index..end];
            match defang(token) {
                Some(defanged) => out.push_str(&defanged),
                None => out.push_str(token),
            }
            copied = end;
            index = end;
            continue;
        }
        index += text[index..].chars().next().map_or(1, char::len_utf8);
    }
    out.push_str(&text[copied..]);
    out
}

/// `https?://` followed by at least one character that is not whitespace or a
/// quoting delimiter. The whole match passes through unchanged.
fn match_scheme(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.len() < start + 8 {
        return None;
    }
    if !bytes[start..start + 4].eq_ignore_ascii_case(b"http") {
        return None;
    }
    let mut cursor = start + 4;
    if bytes[cursor].eq_ignore_ascii_case(&b's') {
        cursor += 1;
    }
    if bytes.get(cursor..cursor + 3) != Some(b"://".as_slice()) {
        return None;
    }
    cursor += 3;
    let mut end = None;
    for (offset, ch) in text[cursor..].char_indices() {
        if is_js_space(ch) || matches!(ch, '<' | '>' | '"' | '\'' | '`') {
            break;
        }
        end = Some(cursor + offset + ch.len_utf8());
    }
    end
}

/// `[A-Za-z0-9][A-Za-z0-9.-]*[A-Za-z0-9]`, greedy, with the trailing
/// character giving back any dot or hyphen.
fn match_word(text: &str, start: usize) -> Option<usize> {
    let first = *text.as_bytes().get(start)?;
    if !first.is_ascii_alphanumeric() {
        return None;
    }
    let mut cursor = start + 1;
    let mut end = cursor;
    while let Some(&byte) = text.as_bytes().get(cursor) {
        if byte.is_ascii_alphanumeric() {
            cursor += 1;
            end = cursor;
        } else if byte == b'.' || byte == b'-' {
            cursor += 1;
        } else {
            break;
        }
    }
    (end > start + 1).then_some(end)
}

/// The last-dot rewrite for a domain token, or `None` when the token passes.
fn defang(token: &str) -> Option<String> {
    let cut = token.rfind('.')?;
    if !is_domain(token) {
        return None;
    }
    let tld = &token[cut + 1..];
    let lower = tld.to_ascii_lowercase();
    if is_allowed_tld(&lower) {
        return None;
    }
    if tld.len() != 2 && !is_iana_tld(&lower) {
        return None;
    }
    let mut out = String::with_capacity(token.len() + 3);
    out.push_str(&token[..cut]);
    out.push_str("[.]");
    out.push_str(&token[cut + 1..]);
    Some(out)
}

/// `^(?:[A-Za-z0-9-]+\.)+[A-Za-z]{2,24}$` from the Node copy.
fn is_domain(token: &str) -> bool {
    let Some((head, tld)) = token.rsplit_once('.') else {
        return false;
    };
    if !(2..=24).contains(&tld.len()) || !tld.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return false;
    }
    head.split('.').all(|label| {
        !label.is_empty()
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Outputs recorded from the Node relay copy for the same inputs.
    #[test]
    fn node_fixture_outputs_match() {
        let cases = [
            ("readme.md", "readme[.]md"),
            ("queued fix readme.md links", "queued fix readme[.]md links"),
            (
                "see https://example.com/a.md",
                "see https://example.com/a.md",
            ),
            ("package.json", "package.json"),
            ("notes.sh", "notes[.]sh"),
            ("backup.zip", "backup[.]zip"),
            ("example.zz", "example[.]zz"),
            ("C:\\Users\\x\\readme.md", "C:\\Users\\x\\readme[.]md"),
            ("2.37.2", "2.37.2"),
            ("", ""),
            ("example.com", "example.com"),
            ("example.nl", "example.nl"),
            ("EXAMPLE.COM", "EXAMPLE.COM"),
            ("example.MD", "example[.]MD"),
            ("readme.md.", "readme[.]md."),
            ("a.b.c.md", "a.b.c[.]md"),
            ("my-file.md", "my-file[.]md"),
            (
                "see \"https://example.com/x.md\"",
                "see \"https://example.com/x.md\"",
            ),
            ("(https://example.com/x.md)", "(https://example.com/x.md)"),
            ("foo..bar.md", "foo..bar.md"),
            ("x-readme.md", "x-readme[.]md"),
            ("readme.md, then next.md", "readme[.]md, then next[.]md"),
            ("https://example.com", "https://example.com"),
            ("http://foo", "http://foo"),
            ("HTTP://EXAMPLE.COM/a.md", "HTTP://EXAMPLE.COM/a.md"),
            ("version 1.2.3", "version 1.2.3"),
            ("node_modules/foo.js", "node_modules/foo[.]js"),
            ("foo.js", "foo[.]js"),
            ("\"readme.md\"", "\"readme[.]md\""),
            ("[readme.md]", "[readme[.]md]"),
            ("readme.md\nnext.md", "readme[.]md\nnext[.]md"),
            ("caf\u{e9}.md", "caf\u{e9}.md"),
            ("readme\u{3002}md", "readme\u{3002}md"),
            ("1st.md", "1st[.]md"),
            ("-readme.md", "-readme[.]md"),
            ("readme-.md", "readme-[.]md"),
            ("a.md", "a[.]md"),
            ("readme.md/path", "readme[.]md/path"),
            ("e.g.", "e.g."),
            ("example.co", "example.co"),
            ("example.pl", "example.pl"),
            ("example.xyz", "example[.]xyz"),
            ("example.notatld", "example.notatld"),
            ("www.example.com/path?q=1", "www.example.com/path?q=1"),
            ("user@example.com", "user@example.com"),
            ("mailto:foo.md", "mailto:foo[.]md"),
            (
                "https://example.com/a.md?x=1",
                "https://example.com/a.md?x=1",
            ),
            ("https://example.com/a.md`", "https://example.com/a.md`"),
            ("a.b", "a.b"),
            ("http://", "http://"),
            ("HTTPS://EXAMPLE.COM/A.MD", "HTTPS://EXAMPLE.COM/A.MD"),
        ];
        for (input, expected) in cases {
            assert_eq!(link_safe(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn allowlist_membership_matches_the_node_set() {
        assert!(is_allowed_tld("com"));
        assert!(is_allowed_tld("nl"));
        assert!(!is_allowed_tld("md"));
        assert_eq!(ALLOWED_TLDS.len(), 68);
        assert!(ALLOWED_TLDS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn every_allowed_tld_is_a_real_iana_tld() {
        assert!(is_iana_tld("md"));
        assert!(is_iana_tld("xyz"));
        assert!(!is_iana_tld("json"));
        for tld in ALLOWED_TLDS {
            assert!(
                is_iana_tld(tld),
                "allowlisted {tld} is not in the IANA table"
            );
        }
    }

    #[test]
    fn rewrite_is_idempotent() {
        let once = link_safe("readme.md at https://example.com/a.json");
        assert_eq!(once, "readme[.]md at https://example.com/a.json");
        assert_eq!(link_safe(&once), once);
    }
}
