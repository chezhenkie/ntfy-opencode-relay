//! Session display helpers shared by routing and acks.
//!
//! `sessionTitle` in `ntfy-inbox-relay.mjs`: a trimmed title, or the first
//! eight characters of the id.

use ntfy_relay_core::text::trim_js;

pub fn short_session_id(id: &str) -> String {
    id.chars().take(8).collect()
}

pub fn session_title(id: &str, title: Option<&str>) -> String {
    let title = title.map(trim_js).unwrap_or("");
    if title.is_empty() {
        short_session_id(id)
    } else {
        title.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_fall_back_to_the_short_id() {
        assert_eq!(
            session_title("ses_abcdef123", Some("  Fix links  ")),
            "Fix links"
        );
        assert_eq!(session_title("ses_abcdef123", Some("   ")), "ses_abcd");
        assert_eq!(session_title("ses_abcdef123", None), "ses_abcd");
    }
}
