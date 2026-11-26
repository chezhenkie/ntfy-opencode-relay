//! `GET /api/event` subscription.
//!
//! The OpenCode event stream is `text/event-stream` and shares the server's
//! Basic auth. Repeat the `types` parameter for every type; a comma-separated
//! list is rejected. The header timeout covers connect and the response status
//! line only; the body may stay open for hours.
//!
//! `EventSource` is a seam so the daemon can hand `outbound` a fake stream and
//! so the rediscovering wrapper can swap the client after a service restart.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use url::form_urlencoded;

use crate::api::{ApiError, BaseUrl, HttpApi};

/// Same header budget the ntfy SSE reader uses.
pub const EVENT_HEADER_TIMEOUT: Duration = Duration::from_secs(30);
/// A 2xx stream that delivers nothing for this long is treated as dead and
/// resubscribed. Twice the ntfy keepalive default of 45 s.
pub const EVENT_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// A non-2xx error body may not hold the reader open; the task must restart.
pub const ERROR_DETAIL_TIMEOUT: Duration = Duration::from_secs(10);

/// Error detail kept from a non-2xx body.
const ERROR_DETAIL_CHARS: usize = 300;

pub struct EventStream {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub chunks: BoxStream<'static, Result<Bytes, ApiError>>,
}

impl EventStream {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[async_trait]
pub trait EventSource: Send + Sync {
    async fn subscribe(&self, types: &[&str]) -> Result<EventStream, ApiError>;
}

/// `/api/event?types=a&types=b`, built with one encoded parameter per type.
pub fn event_url(base: &BaseUrl, types: &[&str]) -> String {
    let mut query = form_urlencoded::Serializer::new(String::new());
    for kind in types {
        query.append_pair("types", kind);
    }
    format!("{}?{}", base.url("/api/event"), query.finish())
}

#[async_trait]
impl EventSource for HttpApi {
    async fn subscribe(&self, types: &[&str]) -> Result<EventStream, ApiError> {
        let url = event_url(self.base(), types);
        let mut builder = self.client_ref().get(&url);
        if let Some(auth) = self.auth_header() {
            builder = builder.header("authorization", auth);
        }
        let response = tokio::time::timeout(EVENT_HEADER_TIMEOUT, builder.send())
            .await
            .map_err(|_| {
                ApiError::new(format!(
                    "GET /api/event: header timeout after {} ms",
                    EVENT_HEADER_TIMEOUT.as_millis()
                ))
            })?
            .map_err(|error| ApiError::new(format!("GET /api/event: {error}")))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        if !(200..300).contains(&status) {
            let detail = read_detail(response).await;
            let message = if detail.is_empty() {
                format!("GET /api/event: HTTP {status}")
            } else {
                format!("GET /api/event: HTTP {status} {detail}")
            };
            return Err(ApiError::with_status(status, message));
        }
        Ok(EventStream {
            status,
            headers,
            chunks: response
                .bytes_stream()
                .map(|chunk| chunk.map_err(|error| ApiError::new(format!("event body: {error}"))))
                .boxed(),
        })
    }
}

async fn read_detail(response: reqwest::Response) -> String {
    match tokio::time::timeout(ERROR_DETAIL_TIMEOUT, read_detail_body(response)).await {
        Ok(detail) => detail,
        Err(_) => "detail timed out".to_string(),
    }
}

async fn read_detail_body(response: reqwest::Response) -> String {
    let mut buffer = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else {
            break;
        };
        let room = ERROR_DETAIL_CHARS.saturating_sub(buffer.len());
        if room == 0 {
            break;
        }
        buffer.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }
    String::from_utf8_lossy(&buffer).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_url_repeats_the_type_parameter() {
        let base = BaseUrl::parse("http://127.0.0.1:4096").unwrap();
        assert_eq!(
            event_url(&base, &["session.text.ended", "permission.asked"]),
            "http://127.0.0.1:4096/api/event?types=session.text.ended&types=permission.asked"
        );
    }

    #[test]
    fn a_base_url_with_a_path_is_refused() {
        // A path prefix used to double the /api segment; the base is now the
        // origin only.
        assert!(BaseUrl::parse("http://127.0.0.1:4096/api").is_err());
    }
}
