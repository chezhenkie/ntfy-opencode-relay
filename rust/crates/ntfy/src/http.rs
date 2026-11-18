//! HTTP transport seam.
//!
//! Requests are plain data so wire assertions need no socket. The production
//! transport is reqwest over rustls with proxies disabled and redirects left
//! to the caller: the relay ignored `HTTP_PROXY` through Node fetch, and the
//! attachment fetch re-checks HTTPS on every hop itself.
//!
//! Response bodies are bounded by `max_response_bytes` so a declared or
//! streamed attachment cannot grow memory without limit.

use std::fmt;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;

/// One transport rule for publish and attachment fetch: HTTPS everywhere, and
/// plain HTTP only for a loopback host. The config loader applies the same
/// rule to `ntfy.server`.
pub fn transport_allowed(url: &url::Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => host_is_loopback(url),
        _ => false,
    }
}

fn host_is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub max_response_bytes: Option<u64>,
}

impl HttpRequest {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: Bytes::new(),
            max_response_bytes: None,
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    pub fn max_response_bytes(mut self, limit: u64) -> Self {
        self.max_response_bytes = Some(limit);
        self
    }

    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug)]
pub enum HttpError {
    /// The request could not be built, for example an invalid header value.
    Request(String),
    /// The exchange failed before a complete response was received.
    Transport(String),
    /// The configured response cap was reached.
    TooLarge { limit: u64 },
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(detail) => write!(f, "http request: {detail}"),
            Self::Transport(detail) => write!(f, "http transport: {detail}"),
            Self::TooLarge { limit } => write!(f, "http response exceeds {limit} bytes"),
        }
    }
}

impl std::error::Error for HttpError {}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError>;
}

/// reqwest-backed transport. Build once per process and share it.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, HttpError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|error| HttpError::Transport(error.to_string()))?;
        Ok(Self { client })
    }

    /// Shared client for streaming responses in `stream.rs`.
    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

impl fmt::Debug for ReqwestTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReqwestTransport")
    }
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
        };
        let mut builder = self.client.request(method, request.url.as_str());
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let response = builder
            .body(request.body)
            .send()
            .await
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
        let limit = request.max_response_bytes;
        if let (Some(limit), Some(length)) = (limit, response.content_length()) {
            if length > limit {
                return Err(HttpError::TooLarge { limit });
            }
        }

        let mut body = BytesMut::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| HttpError::Transport(error.to_string()))?;
            if let Some(limit) = limit {
                if body.len() as u64 + chunk.len() as u64 > limit {
                    return Err(HttpError::TooLarge { limit });
                }
            }
            body.extend_from_slice(&chunk);
        }

        Ok(HttpResponse {
            status,
            headers,
            body: body.freeze(),
        })
    }
}
