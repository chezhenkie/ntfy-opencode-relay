//! Streaming HTTP for the SSE subscription.
//!
//! `Transport` buffers a whole response, which is right for JSON and file
//! writes but wrong for an event stream. This second seam opens a response and
//! yields its body in chunks. The header timeout covers connect, TLS, and the
//! response status line only; the body may stay open for hours.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use serde_json::Value;

use crate::http::{HttpError, HttpRequest, ReqwestTransport};
use crate::sse::SseParser;

pub struct StreamResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub chunks: BoxStream<'static, Result<Bytes, HttpError>>,
}

impl StreamResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[async_trait]
pub trait StreamTransport: Send + Sync {
    async fn open(
        &self,
        request: HttpRequest,
        header_timeout: Duration,
    ) -> Result<StreamResponse, HttpError>;
}

#[async_trait]
impl StreamTransport for ReqwestTransport {
    async fn open(
        &self,
        request: HttpRequest,
        header_timeout: Duration,
    ) -> Result<StreamResponse, HttpError> {
        let method = match request.method {
            crate::http::Method::Get => reqwest::Method::GET,
            crate::http::Method::Post => reqwest::Method::POST,
            crate::http::Method::Put => reqwest::Method::PUT,
        };
        let mut builder = self.client().request(method, request.url.as_str());
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let response = tokio::time::timeout(header_timeout, builder.body(request.body).send())
            .await
            .map_err(|_| {
                HttpError::Transport(format!(
                    "header timeout after {} ms",
                    header_timeout.as_millis()
                ))
            })?
            .map_err(|error| HttpError::Transport(error.to_string()))?;
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
        Ok(StreamResponse {
            status,
            headers,
            chunks: response
                .bytes_stream()
                .map(|chunk| chunk.map_err(|error| HttpError::Transport(error.to_string())))
                .boxed(),
        })
    }
}

/// Reads an SSE body to its end, calling `on_event` for every JSON frame.
/// Frames the parser drops (comments, malformed JSON) never reach the caller.
pub async fn consume_sse<F>(mut response: StreamResponse, mut on_event: F) -> Result<(), HttpError>
where
    F: FnMut(Value),
{
    let mut parser = SseParser::new();
    while let Some(chunk) = response.chunks.next().await {
        for event in parser.push(&chunk?) {
            on_event(event);
        }
    }
    for event in parser.finish() {
        on_event(event);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    fn chunked(parts: Vec<Result<&'static [u8], HttpError>>) -> StreamResponse {
        StreamResponse {
            status: 200,
            headers: Vec::new(),
            chunks: stream::iter(
                parts
                    .into_iter()
                    .map(|part| part.map(Bytes::from_static))
                    .collect::<Vec<_>>(),
            )
            .boxed(),
        }
    }

    #[tokio::test]
    async fn consume_sse_reassembles_frames_and_skips_comments() {
        let response = chunked(vec![
            Ok(b"data: {\"id\":\"a\",".as_slice()),
            Ok(b"\"event\":\"message\"}\r\n\r\n: keepalive\r\n".as_slice()),
            Ok(b"data: not-json\n\ndata: {\"id\":\"b\"}\n\n".as_slice()),
        ]);
        let mut ids = Vec::new();
        consume_sse(response, |value| {
            ids.push(value["id"].as_str().unwrap_or_default().to_string());
        })
        .await
        .unwrap();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn consume_sse_surfaces_a_transport_error() {
        let response = chunked(vec![
            Ok(b"data: {}\n\n".as_slice()),
            Err(HttpError::Transport("reset".to_string())),
        ]);
        let error = consume_sse(response, |_| {}).await.unwrap_err();
        assert!(error.to_string().contains("reset"), "{error}");
    }
}
