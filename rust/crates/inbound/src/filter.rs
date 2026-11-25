//! Inbound message shape and the acceptance filter.
//!
//! Mirrors `shouldAcceptMessage` in `ntfy-inbox-relay.mjs`. The check order
//! matters: the tests pin the first rejection reason, and the replay cursor is
//! compared in epoch seconds while queue age is judged in milliseconds at
//! enqueue time.

use ntfy_relay_core::text::is_js_space;
use serde::{Deserialize, Serialize};

/// `STATE_SKIP_SECONDS`: how far behind the cursor a message may sit.
pub const STATE_SKIP_SECONDS: i64 = 60;
/// A message may sit at most this far ahead of the local clock.
pub const FUTURE_SKEW_SECONDS: i64 = 60;
/// Metadata caps keep a 1000-entry queue bounded independently of the body.
pub const MAX_TAGS: usize = 32;
pub const MAX_TAG_BYTES: usize = 64;
pub const MAX_ATTACHMENT_NAME_BYTES: usize = 255;
pub const MAX_ATTACHMENT_URL_BYTES: usize = 2048;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: Option<i64>,
}

impl Attachment {
    pub fn has_url(&self) -> bool {
        !self.url.is_empty()
    }
}

/// One SSE `message` frame. Extra ntfy fields are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundMessage {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub time: Option<i64>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub attachment: Option<Attachment>,
}

impl InboundMessage {
    pub fn has_attachment(&self) -> bool {
        self.attachment.as_ref().is_some_and(Attachment::has_url)
    }

    pub fn is_own_message(&self) -> bool {
        self.tags.iter().any(|tag| tag == "oc-out")
    }

    pub fn has_body(&self) -> bool {
        !self.message.chars().all(is_js_space)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    NotMessage,
    UnknownTopic,
    OwnMessage,
    Empty,
    Oversized,
    BadTime,
    Future,
    Stale,
    StateStale,
    NoId,
    Duplicate,
}

impl Reject {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotMessage => "not-message",
            Self::UnknownTopic => "unknown-topic",
            Self::OwnMessage => "own-message",
            Self::Empty => "empty",
            Self::Oversized => "oversized",
            Self::BadTime => "bad-time",
            Self::Future => "future",
            Self::Stale => "stale",
            Self::StateStale => "state-stale",
            Self::NoId => "no-id",
            Self::Duplicate => "duplicate",
        }
    }
}

/// Message verdict for one topic at enqueue time.
pub struct AcceptOptions<'a> {
    pub topic: &'a str,
    pub now_ms: i64,
    pub replay_window_minutes: i64,
    pub max_message_bytes: usize,
    pub processed_ids: &'a [String],
    pub last_time: i64,
}

/// `shouldAcceptMessage`. `Ok(())` means the message proceeds; `Err` names the
/// first failing check.
pub fn should_accept(message: &InboundMessage, options: &AcceptOptions<'_>) -> Result<(), Reject> {
    if message.event != "message" {
        return Err(Reject::NotMessage);
    }
    if message.topic != options.topic {
        return Err(Reject::UnknownTopic);
    }
    if message.is_own_message() {
        return Err(Reject::OwnMessage);
    }
    if !message.has_body() && !message.has_attachment() {
        return Err(Reject::Empty);
    }
    if message.message.len() > options.max_message_bytes {
        return Err(Reject::Oversized);
    }
    if message.tags.len() > MAX_TAGS || message.tags.iter().any(|tag| tag.len() > MAX_TAG_BYTES) {
        return Err(Reject::Oversized);
    }
    if let Some(attachment) = &message.attachment {
        if attachment.name.len() > MAX_ATTACHMENT_NAME_BYTES
            || attachment.url.len() > MAX_ATTACHMENT_URL_BYTES
        {
            return Err(Reject::Oversized);
        }
    }
    let Some(time) = message.time else {
        return Err(Reject::BadTime);
    };
    let now_seconds = options.now_ms.div_euclid(1000);
    if time > now_seconds + FUTURE_SKEW_SECONDS {
        return Err(Reject::Future);
    }
    if time < now_seconds - options.replay_window_minutes * 60 {
        return Err(Reject::Stale);
    }
    if options.last_time > 0 && time < options.last_time - STATE_SKIP_SECONDS {
        return Err(Reject::StateStale);
    }
    if message.id.is_empty() {
        return Err(Reject::NoId);
    }
    if options.processed_ids.iter().any(|id| id == &message.id) {
        return Err(Reject::Duplicate);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntfy_relay_core::state::State;

    fn base() -> InboundMessage {
        InboundMessage {
            id: "m1".to_string(),
            event: "message".to_string(),
            topic: "oc-x".to_string(),
            time: Some(1_700_000_000),
            message: "hi".to_string(),
            ..InboundMessage::default()
        }
    }

    fn options<'a>(ids: &'a [String]) -> AcceptOptions<'a> {
        AcceptOptions {
            topic: "oc-x",
            now_ms: 1_700_000_000_000,
            replay_window_minutes: 60,
            max_message_bytes: 16_384,
            processed_ids: ids,
            last_time: 0,
        }
    }

    #[test]
    fn plain_message_is_accepted() {
        assert_eq!(should_accept(&base(), &options(&[])), Ok(()));
    }

    #[test]
    fn own_outbound_keepalive_and_unknown_topic_are_rejected() {
        let mut message = base();
        message.tags = vec!["oc-out".to_string()];
        assert_eq!(
            should_accept(&message, &options(&[])),
            Err(Reject::OwnMessage)
        );

        let mut message = base();
        message.event = "keepalive".to_string();
        assert_eq!(
            should_accept(&message, &options(&[])),
            Err(Reject::NotMessage)
        );

        let mut options = options(&[]);
        options.topic = "oc-y";
        assert_eq!(should_accept(&base(), &options), Err(Reject::UnknownTopic));
    }

    #[test]
    fn stale_oversized_empty_missing_id_and_duplicate_are_rejected() {
        let mut message = base();
        message.time = Some(1_700_000_000 - 3601);
        assert_eq!(should_accept(&message, &options(&[])), Err(Reject::Stale));

        let mut message = base();
        message.message = "0123456789".to_string();
        let mut capped = options(&[]);
        capped.max_message_bytes = 5;
        assert_eq!(should_accept(&message, &capped), Err(Reject::Oversized));

        let mut message = base();
        message.message = "   ".to_string();
        assert_eq!(should_accept(&message, &options(&[])), Err(Reject::Empty));

        let mut message = base();
        message.id.clear();
        assert_eq!(should_accept(&message, &options(&[])), Err(Reject::NoId));

        let ids = vec!["m1".to_string()];
        assert_eq!(
            should_accept(&base(), &options(&ids)),
            Err(Reject::Duplicate)
        );

        let mut cursor = options(&[]);
        cursor.last_time = 1_700_000_000 + 120;
        assert_eq!(should_accept(&base(), &cursor), Err(Reject::StateStale));
    }

    #[test]
    fn missing_time_is_bad_time() {
        let mut message = base();
        message.time = None;
        assert_eq!(should_accept(&message, &options(&[])), Err(Reject::BadTime));
    }

    #[test]
    fn a_future_time_is_rejected() {
        let mut message = base();
        message.time = Some(1_700_000_060);
        assert_eq!(should_accept(&message, &options(&[])), Ok(()));

        message.time = Some(1_700_000_061);
        assert_eq!(should_accept(&message, &options(&[])), Err(Reject::Future));
    }

    #[test]
    fn oversize_metadata_is_rejected() {
        let mut message = base();
        message.tags = (0..=MAX_TAGS).map(|index| format!("t{index}")).collect();
        assert_eq!(
            should_accept(&message, &options(&[])),
            Err(Reject::Oversized)
        );

        let mut message = base();
        message.tags = vec!["x".repeat(MAX_TAG_BYTES + 1)];
        assert_eq!(
            should_accept(&message, &options(&[])),
            Err(Reject::Oversized)
        );

        let mut message = base();
        message.attachment = Some(Attachment {
            url: "https://example.test/file".to_string(),
            name: "n".repeat(MAX_ATTACHMENT_NAME_BYTES + 1),
            size: None,
        });
        assert_eq!(
            should_accept(&message, &options(&[])),
            Err(Reject::Oversized)
        );

        let mut message = base();
        message.attachment = Some(Attachment {
            url: format!(
                "https://example.test/{}",
                "u".repeat(MAX_ATTACHMENT_URL_BYTES)
            ),
            name: "a.png".to_string(),
            size: None,
        });
        assert_eq!(
            should_accept(&message, &options(&[])),
            Err(Reject::Oversized)
        );
    }

    #[test]
    fn attachment_only_message_is_accepted() {
        let mut message = base();
        message.message.clear();
        message.attachment = Some(Attachment {
            url: "https://example.test/file".to_string(),
            name: "a.png".to_string(),
            size: None,
        });
        assert_eq!(should_accept(&message, &options(&[])), Ok(()));
    }

    #[test]
    fn delayed_message_is_judged_at_enqueue_time() {
        let mut message = base();
        message.time = Some(1_700_000_000 - 3500);
        let mut options = options(&[]);
        options.replay_window_minutes = 60;
        assert_eq!(should_accept(&message, &options), Ok(()));

        options.now_ms += 3_600_000;
        assert_eq!(should_accept(&message, &options), Err(Reject::Stale));
    }

    #[test]
    fn restart_cursor_rejects_a_replayed_id() {
        let mut state = State::new();
        state.record_processed("oc-x", "m1", 1_700_000_000);
        let ids = state.topics["oc-x"].last_ids.clone();
        let mut options = options(&ids);
        options.last_time = state.topics["oc-x"].last_time;
        assert_eq!(should_accept(&base(), &options), Err(Reject::Duplicate));
    }
}
