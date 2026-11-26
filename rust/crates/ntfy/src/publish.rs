//! The three publish styles, one per existing caller.
//!
//! - JSON text: plugin parity. POST the base URL with the topic in the body.
//! - Header text: relay parity. POST `/<topic>` with `Title`, `Priority`, and
//!   `Tags` headers and a raw body.
//! - File: plugin parity. PUT `/<topic>?title=...` with the bytes as the body.
//!
//! Titles are sanitized and capped for the plugin styles; the relay style
//! only collapses newlines because the caller has already applied its own
//! text policy.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use url::form_urlencoded;

use crate::header::{
    header_text, normalize_tags, sanitize_title, truncate_utf8, Priority, MAX_MESSAGE_BYTES,
    MAX_TITLE_BYTES,
};
use crate::http::{transport_allowed, HttpError, HttpRequest, Method, Transport};

/// Relay `PUBLISH_TIMEOUT_MS`; the plugin uses a configured timeout instead.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);

/// A publish response is a status line at most; a larger body is an error.
pub const MAX_PUBLISH_RESPONSE_BYTES: u64 = 8 * 1024;

/// A validated ntfy base URL with exactly one trailing slash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtfyServer {
    base: String,
    host: String,
}

impl NtfyServer {
    pub fn parse(raw: &str) -> Result<Self, ServerError> {
        if raw.is_empty() {
            return Err(ServerError::Empty);
        }
        if raw.chars().any(|c| c.is_whitespace() || c == '\u{feff}') {
            return Err(ServerError::Whitespace);
        }
        let url = url::Url::parse(raw).map_err(|_| ServerError::InvalidUrl)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ServerError::UnsupportedScheme);
        }
        if !transport_allowed(&url) {
            return Err(ServerError::Plaintext);
        }
        let host = url.host_str().ok_or(ServerError::MissingHost)?.to_string();
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ServerError::QueryOrFragment);
        }
        if !url.username().is_empty() || url.password().is_some() || has_credentials(raw) {
            return Err(ServerError::Credentials);
        }
        Ok(Self {
            base: format!("{}/", raw.trim_end_matches('/')),
            host,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.base
    }

    /// Host that an attachment URL must match.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// `encodeURIComponent(topic)` appended to the normalized base.
    pub fn topic_url(&self, topic: &str) -> String {
        format!("{}{}", self.base, encode_uri_component(topic))
    }
}

impl fmt::Display for NtfyServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerError {
    Empty,
    Whitespace,
    InvalidUrl,
    UnsupportedScheme,
    Plaintext,
    MissingHost,
    QueryOrFragment,
    Credentials,
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = match self {
            Self::Empty => "server must not be empty",
            Self::Whitespace => "server must not contain whitespace",
            Self::InvalidUrl => "server is not a valid URL",
            Self::UnsupportedScheme => "server must start with http:// or https://",
            Self::Plaintext => "server must use https unless the host is loopback",
            Self::MissingHost => "server must include a host",
            Self::QueryOrFragment => "server must not contain a query or fragment",
            Self::Credentials => "server must not contain credentials",
        };
        f.write_str(detail)
    }
}

impl std::error::Error for ServerError {}

#[derive(Debug, Clone, Copy)]
pub struct JsonText<'a> {
    pub topic: &'a str,
    pub title: &'a str,
    pub message: &'a str,
    pub priority: Priority,
    pub tags: Option<&'a [&'a str]>,
}

#[derive(Debug, Clone, Copy)]
pub struct HeaderText<'a> {
    pub topic: &'a str,
    pub title: &'a str,
    pub message: &'a str,
    pub priority: Priority,
    pub tags: &'a [&'a str],
}

#[derive(Debug, Clone)]
pub struct FileUpload<'a> {
    pub topic: &'a str,
    pub title: &'a str,
    pub message: Option<&'a str>,
    pub priority: Priority,
    pub tags: &'a [&'a str],
    pub filename: Option<&'a str>,
    pub content_type: &'a str,
    pub data: bytes::Bytes,
}

/// JSON body field order matches `JSON.stringify` in `ntfy-client.js`.
#[derive(Serialize)]
struct JsonBody<'a> {
    topic: &'a str,
    title: &'a str,
    message: &'a str,
    priority: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags: Option<&'a [&'a str]>,
}

pub fn json_request(
    server: &NtfyServer,
    token: Option<&str>,
    message: &JsonText<'_>,
) -> HttpRequest {
    let title = sanitize_title(message.title);
    let body = JsonBody {
        topic: message.topic,
        title: truncate_utf8(&title, MAX_TITLE_BYTES),
        message: truncate_utf8(message.message, MAX_MESSAGE_BYTES),
        priority: message.priority.as_level(),
        tags: message.tags,
    };
    let serialized = serde_json::to_vec(&body).expect("json body is always serializable");
    let mut request = HttpRequest::new(Method::Post, server.as_str())
        .header("content-type", "application/json")
        .body(serialized);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request
}

pub fn header_request(
    server: &NtfyServer,
    token: Option<&str>,
    message: &HeaderText<'_>,
) -> HttpRequest {
    let mut request = HttpRequest::new(Method::Post, server.topic_url(message.topic));
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    request = request
        .header("Title", header_text(message.title))
        .header("Priority", message.priority.as_name());
    let tags = normalize_tags(message.tags).join(",");
    if !tags.is_empty() {
        request = request.header("Tags", tags);
    }
    request
        .header("content-type", "text/plain;charset=UTF-8")
        .body(message.message.to_string())
}

pub fn file_request(
    server: &NtfyServer,
    token: Option<&str>,
    file: &FileUpload<'_>,
) -> HttpRequest {
    let title = sanitize_title(file.title);
    let mut query = form_urlencoded::Serializer::new(String::new());
    query.append_pair("title", truncate_utf8(&title, MAX_TITLE_BYTES));
    if let Some(message) = file.message {
        query.append_pair("message", truncate_utf8(message, MAX_MESSAGE_BYTES));
    }
    query.append_pair("priority", &file.priority.as_level().to_string());
    if !file.tags.is_empty() {
        query.append_pair("tags", &file.tags.join(","));
    }
    if let Some(filename) = file.filename {
        query.append_pair("filename", filename);
    }
    let url = format!("{}?{}", server.topic_url(file.topic), query.finish());
    let mut request = HttpRequest::new(Method::Put, url)
        .header("content-type", file.content_type)
        .body(file.data.clone());
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request
}

/// `buildSseUrl`: one encoded, comma-separated topic list plus the replay
/// cursor in epoch seconds.
pub fn sse_url(server: &NtfyServer, topics: &[&str], since: i64) -> String {
    let list = topics
        .iter()
        .map(|topic| encode_uri_component(topic))
        .collect::<Vec<_>>()
        .join(",");
    format!("{}{}/sse?since={since}", server.as_str(), list)
}

#[derive(Debug)]
pub enum PublishError {
    Transport(HttpError),
    Status(u16),
    Timeout { millis: u64 },
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "ntfy publish failed: {error}"),
            Self::Status(status) => {
                write!(f, "ntfy server responded with HTTP status {status}")
            }
            Self::Timeout { millis } => {
                write!(f, "ntfy publish timed out after {millis}ms")
            }
        }
    }
}

impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

impl From<HttpError> for PublishError {
    fn from(error: HttpError) -> Self {
        Self::Transport(error)
    }
}

/// One publisher per base URL and token. The transport is injected so the
/// daemon and the MCP command share the same wire code.
#[derive(Clone)]
pub struct Publisher {
    server: NtfyServer,
    token: Option<String>,
    transport: Arc<dyn Transport>,
    timeout: Duration,
}

impl Publisher {
    pub fn new(server: NtfyServer, token: Option<String>, transport: Arc<dyn Transport>) -> Self {
        Self {
            server,
            token,
            transport,
            timeout: PUBLISH_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn server(&self) -> &NtfyServer {
        &self.server
    }

    pub async fn publish_json(&self, message: &JsonText<'_>) -> Result<(), PublishError> {
        let request = json_request(&self.server, self.token.as_deref(), message);
        self.send(request).await
    }

    pub async fn publish_header(&self, message: &HeaderText<'_>) -> Result<(), PublishError> {
        let request = header_request(&self.server, self.token.as_deref(), message);
        self.send(request).await
    }

    pub async fn publish_file(&self, file: &FileUpload<'_>) -> Result<(), PublishError> {
        let request = file_request(&self.server, self.token.as_deref(), file);
        self.send(request).await
    }

    async fn send(&self, mut request: HttpRequest) -> Result<(), PublishError> {
        if request.max_response_bytes.is_none() {
            request.max_response_bytes = Some(MAX_PUBLISH_RESPONSE_BYTES);
        }
        let response = tokio::time::timeout(self.timeout, self.transport.send(request))
            .await
            .map_err(|_| PublishError::Timeout {
                millis: self.timeout.as_millis() as u64,
            })??;
        if !(200..300).contains(&response.status) {
            return Err(PublishError::Status(response.status));
        }
        Ok(())
    }
}

impl fmt::Debug for Publisher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Publisher")
            .field("server", &self.server)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

fn has_credentials(raw: &str) -> bool {
    let Some(scheme_end) = raw.find("://") else {
        return false;
    };
    let authority = &raw[scheme_end + 3..];
    let end = authority.find('/').unwrap_or(authority.len());
    authority[..end].contains('@')
}

/// JavaScript `encodeURIComponent`, used for the topic path segment.
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'A' + value - 10) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_normalizes_one_trailing_slash() {
        assert_eq!(
            NtfyServer::parse("https://ntfy.test").unwrap().as_str(),
            "https://ntfy.test/"
        );
        assert_eq!(
            NtfyServer::parse("https://ntfy.test/prefix//")
                .unwrap()
                .as_str(),
            "https://ntfy.test/prefix/"
        );
    }

    #[test]
    fn server_exposes_the_attachment_host() {
        assert_eq!(
            NtfyServer::parse("https://ntfy.test/prefix/")
                .unwrap()
                .host(),
            "ntfy.test"
        );
        assert_eq!(
            NtfyServer::parse("http://127.0.0.1:8810").unwrap().host(),
            "127.0.0.1"
        );
    }

    #[test]
    fn server_rejects_non_base_urls() {
        assert_eq!(NtfyServer::parse(""), Err(ServerError::Empty));
        assert_eq!(
            NtfyServer::parse("https://ntfy.test?x=1"),
            Err(ServerError::QueryOrFragment)
        );
        assert_eq!(
            NtfyServer::parse("ftp://ntfy.test"),
            Err(ServerError::UnsupportedScheme)
        );
        assert_eq!(
            NtfyServer::parse("https://user:pw@ntfy.test"),
            Err(ServerError::Credentials)
        );
        assert_eq!(
            NtfyServer::parse("https://ntfy test"),
            Err(ServerError::Whitespace)
        );
        assert_eq!(
            NtfyServer::parse("http://ntfy.test"),
            Err(ServerError::Plaintext)
        );
        assert!(NtfyServer::parse("http://127.0.0.1:8810").is_ok());
        assert!(NtfyServer::parse("http://localhost:8810").is_ok());
        assert!(NtfyServer::parse("http://[::1]:8810").is_ok());
    }

    #[test]
    fn topic_encoding_matches_encode_uri_component() {
        let server = NtfyServer::parse("https://ntfy.test").unwrap();
        assert_eq!(server.topic_url("ntfy-proj"), "https://ntfy.test/ntfy-proj");
        assert_eq!(
            server.topic_url("a b!*'()~"),
            "https://ntfy.test/a%20b!*'()~"
        );
        assert_eq!(server.topic_url("caf\u{e9}"), "https://ntfy.test/caf%C3%A9");
    }

    #[test]
    fn sse_url_matches_the_relay_builder() {
        let server = NtfyServer::parse("https://ntfy.test").unwrap();
        assert_eq!(
            sse_url(&server, &["oc-a", "oc-b"], 123),
            "https://ntfy.test/oc-a,oc-b/sse?since=123"
        );
        assert_eq!(
            sse_url(&server, &["ntfy-proj"], 1_700_000_000),
            "https://ntfy.test/ntfy-proj/sse?since=1700000000"
        );
    }

    #[test]
    fn json_body_field_order_matches_json_stringify() {
        let server = NtfyServer::parse("https://ntfy.test").unwrap();
        let request = json_request(
            &server,
            Some("tk_x"),
            &JsonText {
                topic: "ntfy-proj",
                title: "ntfy-proj",
                message: "queued to readme.md",
                priority: Priority::Default,
                tags: Some(&["oc-out"]),
            },
        );
        assert_eq!(
            std::str::from_utf8(&request.body).unwrap(),
            r#"{"topic":"ntfy-proj","title":"ntfy-proj","message":"queued to readme.md","priority":3,"tags":["oc-out"]}"#
        );
    }
}
