//! One outbound push, serialized through the shared ntfy publisher.
//!
//! Text uses the JSON POST and files use the PUT with query metadata, matching
//! `ntfy-client.js` in the patched plugin. Titles and budgets stay in
//! `rust/crates/ntfy`; this module only picks the style.

use bytes::Bytes;
use ntfy_relay_ntfy::header::Priority;
use ntfy_relay_ntfy::publish::{FileUpload, JsonText, PublishError, Publisher};

#[derive(Debug, Clone)]
pub struct OutboundFile {
    pub filename: String,
    pub content_type: String,
    pub data: Bytes,
}

#[derive(Debug, Clone)]
pub struct OutboundMessage {
    /// Short label for logs, e.g. `session.text`.
    pub kind: &'static str,
    pub topic: String,
    pub title: String,
    pub message: String,
    pub tags: Vec<&'static str>,
    pub priority: Priority,
    pub file: Option<OutboundFile>,
}

pub async fn send(publisher: &Publisher, message: &OutboundMessage) -> Result<(), PublishError> {
    match &message.file {
        Some(file) => {
            let upload = FileUpload {
                topic: &message.topic,
                title: &message.title,
                message: if message.message.is_empty() {
                    None
                } else {
                    Some(&message.message)
                },
                priority: message.priority,
                tags: &message.tags,
                filename: Some(&file.filename),
                content_type: &file.content_type,
                data: file.data.clone(),
            };
            publisher.publish_file(&upload).await
        }
        None => {
            let text = JsonText {
                topic: &message.topic,
                title: &message.title,
                message: &message.message,
                priority: message.priority,
                tags: if message.tags.is_empty() {
                    None
                } else {
                    Some(&message.tags)
                },
            };
            publisher.publish_json(&text).await
        }
    }
}
