//! Local OpenCode HTTP client.
//!
//! The Node relay shells out to `opencode api ...`; the Rust relay talks to the
//! same routes directly. Wire shapes come from the running server's
//! `openapi.json` (`Project`, `Session.Info`, `SessionActive`, `SessionsResponse`,
//! `V2Event.*`) and from captured responses under `tests/fixtures`.
//!
//! Responses are bounded by `MAX_RESPONSE_BYTES`, so a runaway session list
//! cannot grow memory without limit. A call has one timeout; retry policy stays
//! with the caller, which knows whether an attempt is safe to repeat.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Matches the relay's `opencode.timeoutMs`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound for any single JSON response. Session lists are small; this only
/// stops a broken server from streaming without limit.
pub const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

/// Error detail kept from a non-2xx body.
const ERROR_DETAIL_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    status: Option<u16>,
    message: String,
    /// The connection itself failed, so the request provably never reached the
    /// server and repeating it is safe.
    connect: bool,
}

impl ApiError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
            connect: false,
        }
    }

    pub fn with_status(status: u16, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            message: message.into(),
            connect: false,
        }
    }

    pub fn connect(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
            connect: true,
        }
    }

    pub fn code(&self) -> Option<u16> {
        self.status
    }

    pub fn is_connect(&self) -> bool {
        self.connect
    }

    pub fn is_not_found(&self) -> bool {
        self.status == Some(404)
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// `opencode.baseUrl` and the transport rule live in core so the relay config,
/// the service registration and the ntfy client share one check.
pub use ntfy_relay_core::net::{BaseUrl, BaseUrlError};

/// Basic auth for the local server: user `opencode`, password from
/// `~/.config/opencode/service.json` (the mechanism the `opencode api` CLI uses).
#[derive(Clone, PartialEq, Eq)]
pub struct BasicAuth {
    header: String,
}

impl BasicAuth {
    pub fn new(user: &str, password: &str) -> Self {
        let mut raw = String::with_capacity(user.len() + password.len() + 1);
        raw.push_str(user);
        raw.push(':');
        raw.push_str(password);
        Self {
            header: format!("Basic {}", BASE64.encode(raw)),
        }
    }

    pub fn opencode(password: &str) -> Self {
        Self::new("opencode", password)
    }

    pub fn from_service_json(bytes: &[u8]) -> Result<Self, ApiError> {
        #[derive(Deserialize)]
        struct ServiceFile {
            #[serde(default)]
            password: Option<String>,
        }
        let service: ServiceFile = serde_json::from_slice(bytes)
            .map_err(|error| ApiError::new(format!("service file is not valid JSON: {error}")))?;
        match service.password {
            Some(password) if !password.is_empty() => Ok(Self::opencode(&password)),
            _ => Err(ApiError::new("service file has no password")),
        }
    }

    pub fn header(&self) -> &str {
        &self.header
    }
}

impl fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BasicAuth(<redacted>)")
    }
}

/// Registration written by the shared OpenCode service at
/// `~/.local/state/opencode/service.json`. The URL and password belong to the
/// running service process; the file is rewritten on every service start.
pub const SERVICE_STATE_PATH: &str = ".local/state/opencode/service.json";

/// One parsed service registration. Password stays private; callers get auth.
pub struct ServiceRegistration {
    base_url: BaseUrl,
    pid: u32,
    password: String,
}

impl ServiceRegistration {
    pub fn state_path(home: &Path) -> PathBuf {
        home.join(SERVICE_STATE_PATH)
    }

    /// Reads a registration file. A missing file is `None`; malformed content
    /// or a missing URL, pid, or password is an error.
    pub fn load(path: &Path) -> Result<Option<Self>, ApiError> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(ApiError::new(format!(
                    "cannot read service registration {}: {error}",
                    path.display()
                )))
            }
        };
        Self::from_json(&bytes)
            .map(Some)
            .map_err(|error| ApiError::new(format!("{}: {}", path.display(), error.message())))
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, ApiError> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            url: String,
            #[serde(default)]
            pid: u64,
            #[serde(default)]
            password: String,
        }
        let raw: Raw = serde_json::from_slice(bytes)
            .map_err(|error| ApiError::new(format!("not valid JSON: {error}")))?;
        if raw.password.is_empty() {
            return Err(ApiError::new("registration has no password"));
        }
        let pid = u32::try_from(raw.pid)
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| ApiError::new("registration has no valid pid"))?;
        let base_url =
            BaseUrl::parse(&raw.url).map_err(|error| ApiError::new(format!("url: {error}")))?;
        Ok(Self {
            base_url,
            pid,
            password: raw.password,
        })
    }

    pub fn base_url(&self) -> &BaseUrl {
        &self.base_url
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn auth(&self) -> BasicAuth {
        BasicAuth::opencode(&self.password)
    }
}

impl fmt::Debug for ServiceRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceRegistration")
            .field("base_url", &self.base_url)
            .field("pid", &self.pid)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProjectInfo {
    pub id: String,
    #[serde(default)]
    pub canonical: Option<String>,
    #[serde(default)]
    pub time: ProjectTime,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct ProjectTime {
    #[serde(default)]
    pub active: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(rename = "projectID", default)]
    pub project_id: Option<String>,
    /// Set on child sessions; subagent suppression reads this in `outbound`.
    #[serde(rename = "parentID", default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub time: SessionTime,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct SessionTime {
    #[serde(default)]
    pub updated: i64,
    /// Written by the desktop client when a session is opened or used; API
    /// prompts do not touch it. Zero when the server does not report it.
    #[serde(default)]
    pub viewed: i64,
}

/// Provider model reference, trimmed and validated the way
/// `normalizeModel` in `ntfy-opencode-adapter.mjs` does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl ModelRef {
    pub fn new(provider_id: &str, id: &str, variant: Option<&str>) -> Option<Self> {
        let provider_id = provider_id.trim();
        let id = id.trim();
        if provider_id.is_empty() || id.is_empty() {
            return None;
        }
        Some(Self {
            provider_id: provider_id.to_string(),
            id: id.to_string(),
            variant: variant
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        })
    }
}

/// One row from `GET /api/session/{id}/message`. `time.completed` is absent
/// while a message is still streamed, so a backfill skips it until the next
/// reconnect.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MessageInfo {
    pub id: String,
    #[serde(default)]
    pub time: MessageTime,
    #[serde(default)]
    pub content: Vec<MessageContent>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct MessageTime {
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub completed: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MessageContent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
}

impl MessageInfo {
    pub fn completed_at(&self) -> Option<i64> {
        self.time.completed
    }

    /// Text parts in wire order. Reasoning and tool parts are dropped.
    pub fn text_parts(&self) -> Vec<String> {
        self.content
            .iter()
            .filter(|part| part.kind == "text")
            .filter_map(|part| part.text.clone())
            .collect()
    }
}

/// One newest-first page of assistant messages plus the cursor for the next
/// page. `next` is `None` on the last page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessagePage {
    pub messages: Vec<MessageInfo>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PermissionInfo {
    pub id: String,
}

/// `Permission.Decision`, the three labels the phone commands map to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplyDecision {
    Once,
    Always,
    Reject,
}

/// `Session.Inbox.Delivery`: `queue` waits for the current run, `steer`
/// interrupts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    Queue,
    Steer,
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Steer => "steer",
        }
    }
}

/// `PromptInput.FileAttachment`, narrowed to the two fields the relay sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptFile {
    pub uri: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy)]
pub struct CreateSession<'a> {
    pub directory: &'a str,
    pub agent: &'a str,
    pub model: &'a ModelRef,
    pub title: Option<&'a str>,
}

#[derive(Serialize)]
struct CreateBody<'a> {
    location: Location<'a>,
    agent: &'a str,
    model: &'a ModelRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
}

#[derive(Serialize)]
struct Location<'a> {
    directory: &'a str,
}

#[derive(Serialize)]
struct PromptBody<'a> {
    text: &'a str,
    delivery: Delivery,
    #[serde(skip_serializing_if = "slice_is_empty")]
    files: &'a [PromptFile],
}

#[derive(Serialize)]
struct ReplyBody {
    decision: ReplyDecision,
}

fn slice_is_empty<T>(value: &&[T]) -> bool {
    value.is_empty()
}

/// Every route the relay uses. A fake implementation drives adapter tests and
/// keeps `opencode` free of a socket in unit runs.
#[async_trait]
pub trait Api: Send + Sync {
    async fn projects(&self) -> Result<Vec<ProjectInfo>, ApiError>;

    async fn sessions(&self, project_id: &str, limit: usize) -> Result<Vec<SessionInfo>, ApiError>;

    async fn session(&self, id: &str) -> Result<SessionInfo, ApiError>;

    /// Ids of sessions whose active entry is `running`.
    async fn active_ids(&self) -> Result<Vec<String>, ApiError>;

    /// Newest-first page of assistant messages for one session. The cursor is
    /// opaque and comes from `MessagePage::next`.
    async fn messages(
        &self,
        session_id: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<MessagePage, ApiError>;

    async fn create_session(&self, request: CreateSession<'_>) -> Result<SessionInfo, ApiError>;

    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        delivery: Delivery,
        files: &[PromptFile],
    ) -> Result<(), ApiError>;

    /// Pending permission requests for one session, oldest first is decided by
    /// the caller; the route returns server order.
    async fn permissions(&self, session_id: &str) -> Result<Vec<PermissionInfo>, ApiError>;

    async fn reply_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        decision: ReplyDecision,
    ) -> Result<(), ApiError>;

    /// Cancel one pending form, with the free text the asker receives.
    async fn cancel_form(
        &self,
        session_id: &str,
        form_id: &str,
        message: &str,
    ) -> Result<(), ApiError>;
}

pub struct HttpApi {
    base: BaseUrl,
    auth: Option<BasicAuth>,
    client: reqwest::Client,
    timeout: Duration,
}

impl HttpApi {
    pub fn new(base: BaseUrl) -> Result<Self, ApiError> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| ApiError::new(format!("http client: {error}")))?;
        Ok(Self {
            base,
            auth: None,
            client,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    pub fn with_auth(mut self, auth: BasicAuth) -> Self {
        self.auth = Some(auth);
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn base(&self) -> &BaseUrl {
        &self.base
    }

    pub(crate) fn client_ref(&self) -> &reqwest::Client {
        &self.client
    }

    pub(crate) fn auth_header(&self) -> Option<&str> {
        self.auth.as_ref().map(BasicAuth::header)
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, ApiError> {
        let label = format!("{method} {path}");
        let mut builder = self
            .client
            .request(method, self.base.url(path))
            .timeout(self.timeout);
        if let Some(auth) = &self.auth {
            builder = builder.header("authorization", auth.header());
        }
        if let Some(body) = body {
            builder = builder
                .header("content-type", "application/json")
                .body(body);
        }
        let response = builder.send().await.map_err(|error| {
            let message = format!("{label}: {error}");
            if error.is_connect() {
                ApiError::connect(message)
            } else {
                ApiError::new(message)
            }
        })?;
        let status = response.status().as_u16();
        let bytes = read_bounded(response).await?;
        if !(200..300).contains(&status) {
            let detail = String::from_utf8_lossy(&bytes);
            let detail = clip(detail.trim(), ERROR_DETAIL_CHARS);
            let message = if detail.is_empty() {
                format!("{label}: HTTP {status}")
            } else {
                format!("{label}: HTTP {status} {detail}")
            };
            return Err(ApiError::with_status(status, message));
        }
        Ok(bytes)
    }

    async fn get(&self, path: &str) -> Result<Vec<u8>, ApiError> {
        self.send(reqwest::Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Vec<u8>) -> Result<Vec<u8>, ApiError> {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }
}

#[async_trait]
impl Api for HttpApi {
    async fn projects(&self) -> Result<Vec<ProjectInfo>, ApiError> {
        let bytes = self.get("/api/project").await?;
        decode_projects(&bytes)
    }

    async fn sessions(&self, project_id: &str, limit: usize) -> Result<Vec<SessionInfo>, ApiError> {
        let path = format!(
            "/api/session?project={}&limit={limit}&order=desc&archived=false",
            encode_uri_component(project_id)
        );
        let bytes = self.get(&path).await?;
        decode_sessions(&bytes)
    }

    async fn session(&self, id: &str) -> Result<SessionInfo, ApiError> {
        let path = format!("/api/session/{}", encode_uri_component(id));
        let bytes = self.get(&path).await?;
        decode_session(&bytes)
    }

    async fn active_ids(&self) -> Result<Vec<String>, ApiError> {
        let bytes = self.get("/api/session/active").await?;
        decode_active_ids(&bytes)
    }

    async fn messages(
        &self,
        session_id: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<MessagePage, ApiError> {
        let bytes = self.get(&messages_path(session_id, limit, cursor)).await?;
        decode_messages(&bytes)
    }

    async fn create_session(&self, request: CreateSession<'_>) -> Result<SessionInfo, ApiError> {
        let body = CreateBody {
            location: Location {
                directory: request.directory,
            },
            agent: request.agent,
            model: request.model,
            title: request.title,
        };
        let bytes = serde_json::to_vec(&body)
            .map_err(|error| ApiError::new(format!("create body: {error}")))?;
        let bytes = self.post("/api/session", bytes).await?;
        decode_session(&bytes)
    }

    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        delivery: Delivery,
        files: &[PromptFile],
    ) -> Result<(), ApiError> {
        let body = PromptBody {
            text,
            delivery,
            files,
        };
        let bytes = serde_json::to_vec(&body)
            .map_err(|error| ApiError::new(format!("prompt body: {error}")))?;
        let path = format!("/api/session/{}/prompt", encode_uri_component(session_id));
        self.post(&path, bytes).await?;
        Ok(())
    }

    async fn permissions(&self, session_id: &str) -> Result<Vec<PermissionInfo>, ApiError> {
        let path = format!(
            "/api/session/{}/permission",
            encode_uri_component(session_id)
        );
        let bytes = self.get(&path).await?;
        decode_permissions(&bytes)
    }

    async fn reply_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        decision: ReplyDecision,
    ) -> Result<(), ApiError> {
        let body = serde_json::to_vec(&ReplyBody { decision })
            .map_err(|error| ApiError::new(format!("permission body: {error}")))?;
        let path = format!(
            "/api/session/{}/permission/{}/reply",
            encode_uri_component(session_id),
            encode_uri_component(permission_id)
        );
        self.post(&path, body).await?;
        Ok(())
    }

    /// `Form.Cancel`: a DELETE with the answer as the reason. The server
    /// answers 204, 404 for an unknown form, or 409 for a settled one.
    async fn cancel_form(
        &self,
        session_id: &str,
        form_id: &str,
        message: &str,
    ) -> Result<(), ApiError> {
        let mut path = format!(
            "/api/session/{}/form/{}",
            encode_uri_component(session_id),
            encode_uri_component(form_id)
        );
        if !message.is_empty() {
            path.push_str("?message=");
            path.push_str(&encode_uri_component(message));
        }
        self.send(reqwest::Method::DELETE, &path, None).await?;
        Ok(())
    }
}

async fn read_bounded(response: reqwest::Response) -> Result<Vec<u8>, ApiError> {
    if let Some(length) = response.content_length() {
        if length > MAX_RESPONSE_BYTES {
            return Err(ApiError::new(format!(
                "response exceeds {MAX_RESPONSE_BYTES} bytes"
            )));
        }
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ApiError::new(format!("response body: {error}")))?;
        if body.len() as u64 + chunk.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(ApiError::new(format!(
                "response exceeds {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_payload(bytes: &[u8]) -> Result<Value, ApiError> {
    if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(Value::Null);
    }
    serde_json::from_slice(bytes)
        .map_err(|error| ApiError::new(format!("response is not JSON: {error}")))
}

fn unwrap_data(value: Value) -> Value {
    match value {
        Value::Object(mut object) => match object.remove("data") {
            Some(data) => data,
            None => Value::Object(object),
        },
        other => other,
    }
}

pub fn decode_projects(bytes: &[u8]) -> Result<Vec<ProjectInfo>, ApiError> {
    match unwrap_data(parse_payload(bytes)?) {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .into_iter()
            .map(|item| {
                serde_json::from_value(item)
                    .map_err(|error| ApiError::new(format!("project entry: {error}")))
            })
            .collect(),
        _ => Err(ApiError::new("project list is not an array")),
    }
}

pub fn decode_sessions(bytes: &[u8]) -> Result<Vec<SessionInfo>, ApiError> {
    match unwrap_data(parse_payload(bytes)?) {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .into_iter()
            .map(|item| {
                serde_json::from_value(item)
                    .map_err(|error| ApiError::new(format!("session entry: {error}")))
            })
            .collect(),
        _ => Err(ApiError::new("session list is not an array")),
    }
}

pub fn decode_session(bytes: &[u8]) -> Result<SessionInfo, ApiError> {
    let value = unwrap_data(parse_payload(bytes)?);
    serde_json::from_value(value).map_err(|error| ApiError::new(format!("session: {error}")))
}

pub fn decode_permissions(bytes: &[u8]) -> Result<Vec<PermissionInfo>, ApiError> {
    match unwrap_data(parse_payload(bytes)?) {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .into_iter()
            .map(|item| {
                serde_json::from_value(item)
                    .map_err(|error| ApiError::new(format!("permission entry: {error}")))
            })
            .collect(),
        _ => Err(ApiError::new("permission list is not an array")),
    }
}

pub fn decode_active_ids(bytes: &[u8]) -> Result<Vec<String>, ApiError> {
    match unwrap_data(parse_payload(bytes)?) {
        Value::Null => Ok(Vec::new()),
        Value::Object(entries) => {
            let mut ids: Vec<String> = entries
                .into_iter()
                .filter(|(_, entry)| entry.get("type").and_then(Value::as_str) == Some("running"))
                .map(|(id, _)| id)
                .collect();
            ids.sort();
            Ok(ids)
        }
        _ => Err(ApiError::new("active session map is not an object")),
    }
}

/// The message page keeps `cursor.next` next to `data`, so it cannot use the
/// generic `data` unwrap.
pub fn decode_messages(bytes: &[u8]) -> Result<MessagePage, ApiError> {
    let (items, next) = match parse_payload(bytes)? {
        Value::Null => (Vec::new(), None),
        Value::Array(items) => (items, None),
        Value::Object(mut object) => {
            let items = match object.remove("data") {
                Some(Value::Array(items)) => items,
                Some(Value::Null) | None => Vec::new(),
                Some(_) => return Err(ApiError::new("message list is not an array")),
            };
            let next = object
                .get("cursor")
                .and_then(Value::as_object)
                .and_then(|cursor| cursor.get("next"))
                .and_then(Value::as_str)
                .filter(|next| !next.is_empty())
                .map(str::to_string);
            (items, next)
        }
        _ => return Err(ApiError::new("message page is not an object")),
    };
    let messages = items
        .into_iter()
        .map(|item| {
            serde_json::from_value(item)
                .map_err(|error| ApiError::new(format!("message entry: {error}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MessagePage { messages, next })
}

fn messages_path(session_id: &str, limit: usize, cursor: Option<&str>) -> String {
    let mut path = format!(
        "/api/session/{}/message?type=assistant&limit={limit}",
        encode_uri_component(session_id)
    );
    match cursor {
        // The cursor carries the order; sending `order` next to it is rejected
        // with InvalidCursorError.
        Some(cursor) => {
            path.push_str("&cursor=");
            path.push_str(&encode_uri_component(cursor));
        }
        None => path.push_str("&order=desc"),
    }
    path
}

/// JavaScript `encodeURIComponent`, used for path and query segments.
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

fn clip(value: &str, max_chars: usize) -> &str {
    match value.char_indices().nth(max_chars) {
        Some((index, _)) => &value[..index],
        None => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_normalizes_and_rejects_non_base_urls() {
        assert_eq!(
            BaseUrl::parse("http://127.0.0.1:4096/").unwrap().as_str(),
            "http://127.0.0.1:4096"
        );
        assert_eq!(
            BaseUrl::parse("http://127.0.0.1:4096/api"),
            Err(BaseUrlError::Path)
        );
        assert_eq!(
            BaseUrl::parse("http://192.168.1.20:4096"),
            Err(BaseUrlError::PlainHttp)
        );
        assert_eq!(BaseUrl::parse(""), Err(BaseUrlError::Empty));
        assert_eq!(
            BaseUrl::parse("ftp://127.0.0.1:4096"),
            Err(BaseUrlError::UnsupportedScheme)
        );
        assert_eq!(
            BaseUrl::parse("http://127.0.0.1:4096?x=1"),
            Err(BaseUrlError::QueryOrFragment)
        );
        assert_eq!(
            BaseUrl::parse("http://opencode:pw@127.0.0.1:4096"),
            Err(BaseUrlError::Credentials)
        );
        assert_eq!(
            BaseUrl::parse("http://127.0.0.1 :4096"),
            Err(BaseUrlError::Whitespace)
        );
    }

    #[test]
    fn service_registration_reads_url_pid_and_password() {
        let registration = ServiceRegistration::from_json(
            br#"{"id":"x","version":"2.2.7","url":"http://127.0.0.1:28894","pid":42,"password":"secret"}"#,
        )
        .unwrap();
        assert_eq!(registration.base_url().as_str(), "http://127.0.0.1:28894");
        assert_eq!(registration.pid(), 42);
        assert!(registration.auth().header().starts_with("Basic "));
        assert!(format!("{registration:?}").contains("redacted"));
        assert_eq!(
            ServiceRegistration::state_path(Path::new("/home/x")),
            PathBuf::from("/home/x/.local/state/opencode/service.json")
        );

        assert!(
            ServiceRegistration::from_json(br#"{"url":"http://127.0.0.1:1","pid":1}"#).is_err()
        );
        assert!(ServiceRegistration::from_json(
            br#"{"url":"http://127.0.0.1:1","pid":0,"password":"x"}"#
        )
        .is_err());
        assert!(
            ServiceRegistration::from_json(br#"{"url":"nope","pid":1,"password":"x"}"#).is_err()
        );
        let refused = ServiceRegistration::from_json(
            br#"{"url":"http://192.168.1.20:4096","pid":1,"password":"x"}"#,
        );
        assert!(refused.is_err(), "plain HTTP on a LAN address is refused");
        assert!(refused
            .unwrap_err()
            .message()
            .contains("must use HTTPS unless the host is loopback"));
    }

    #[test]
    fn a_service_file_with_plain_lan_http_names_its_path() {
        let dir = std::env::temp_dir().join(format!("ntfy-service-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("service.json");
        fs::write(
            &path,
            br#"{"url":"http://192.168.1.20:4096","pid":1,"password":"x"}"#,
        )
        .unwrap();
        let error = ServiceRegistration::load(&path).unwrap_err();
        assert!(
            error.message().contains(&path.display().to_string()),
            "{}",
            error.message()
        );
        assert!(error.message().contains("HTTPS"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_path_encodes_the_id() {
        let base = BaseUrl::parse("http://127.0.0.1:4096").unwrap();
        assert_eq!(
            base.url(&format!("/api/session/{}", encode_uri_component("ses_a/b"))),
            "http://127.0.0.1:4096/api/session/ses_a%2Fb"
        );
    }

    #[test]
    fn model_ref_matches_normalize_model() {
        assert_eq!(
            ModelRef::new(" opencode-go ", " deepseek-v4.1-flash ", Some(" max ")),
            Some(ModelRef {
                provider_id: "opencode-go".to_string(),
                id: "deepseek-v4.1-flash".to_string(),
                variant: Some("max".to_string()),
            })
        );
        assert_eq!(
            ModelRef::new("opencode-go", "deepseek-v4.1-flash", Some("  ")),
            Some(ModelRef {
                provider_id: "opencode-go".to_string(),
                id: "deepseek-v4.1-flash".to_string(),
                variant: None,
            })
        );
        assert_eq!(ModelRef::new("", "model", None), None);
        assert_eq!(ModelRef::new("provider", " ", None), None);
    }

    #[test]
    fn basic_auth_header_is_std_base64() {
        let auth = BasicAuth::opencode("secret");
        assert_eq!(auth.header(), "Basic b3BlbmNvZGU6c2VjcmV0");
        assert_eq!(format!("{auth:?}"), "BasicAuth(<redacted>)");
    }

    #[test]
    fn create_body_matches_the_adapter_body() {
        let model = ModelRef::new("opencode-go", "deepseek-v4.1-flash", Some("max")).unwrap();
        let body = CreateBody {
            location: Location {
                directory: "C:\\Users\\example\\projects\\ntfy-proj",
            },
            agent: "build",
            model: &model,
            title: Some("fixture capture"),
        };
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"location":{"directory":"C:\\Users\\example\\projects\\ntfy-proj"},"agent":"build","model":{"providerID":"opencode-go","id":"deepseek-v4.1-flash","variant":"max"},"title":"fixture capture"}"#
        );
    }

    #[test]
    fn prompt_body_matches_send_prompt() {
        let files = [PromptFile {
            uri: "file:///C:/tmp/note.md".to_string(),
            name: "note.md".to_string(),
        }];
        let body = PromptBody {
            text: "queued to note.md",
            delivery: Delivery::Queue,
            files: &files,
        };
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"text":"queued to note.md","delivery":"queue","files":[{"uri":"file:///C:/tmp/note.md","name":"note.md"}]}"#
        );
        let body = PromptBody {
            text: "hello",
            delivery: Delivery::Steer,
            files: &[],
        };
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"text":"hello","delivery":"steer"}"#
        );
    }

    #[test]
    fn permission_list_accepts_both_wrappers() {
        assert_eq!(
            decode_permissions(br#"{"data":[{"id":"per_1"}]}"#).unwrap(),
            vec![PermissionInfo {
                id: "per_1".to_string()
            }]
        );
        assert_eq!(
            decode_permissions(br#"[{"id":"per_2"}]"#).unwrap(),
            vec![PermissionInfo {
                id: "per_2".to_string()
            }]
        );
        assert!(decode_permissions(br#"{"data":null}"#).unwrap().is_empty());
    }

    #[test]
    fn permission_reply_body_uses_the_three_labels() {
        for (decision, wire) in [
            (ReplyDecision::Once, "once"),
            (ReplyDecision::Always, "always"),
            (ReplyDecision::Reject, "reject"),
        ] {
            let body = ReplyBody { decision };
            assert_eq!(
                serde_json::to_string(&body).unwrap(),
                format!(r#"{{"decision":"{wire}"}}"#)
            );
        }
    }

    #[test]
    fn the_message_route_asks_for_assistant_messages_newest_first() {
        assert_eq!(
            messages_path("ses_a", 20, None),
            "/api/session/ses_a/message?type=assistant&limit=20&order=desc"
        );
        assert_eq!(
            messages_path("ses_a/b", 3, Some("cur sor&x")),
            "/api/session/ses_a%2Fb/message?type=assistant&limit=3&cursor=cur%20sor%26x"
        );
    }

    #[test]
    fn message_pages_keep_the_cursor_and_filter_text_parts() {
        let page = decode_messages(
            br#"{"data":[
                {"id":"msg_1","time":{"created":10,"completed":20},
                 "content":[{"type":"reasoning","text":"why"},
                            {"type":"text","text":"first"},
                            {"type":"tool","text":"ignored"},
                            {"type":"text","text":"second"}]},
                {"id":"msg_2","time":{"created":30},"content":[{"type":"text","text":"open"}]}
            ],"cursor":{"previous":"p","next":"n"}}"#,
        )
        .unwrap();
        assert_eq!(page.next.as_deref(), Some("n"));
        assert_eq!(page.messages.len(), 2);
        assert_eq!(page.messages[0].text_parts(), vec!["first", "second"]);
        assert_eq!(page.messages[0].completed_at(), Some(20));
        assert_eq!(page.messages[1].completed_at(), None);
        assert!(page.messages[1].text_parts() == vec!["open"]);
    }

    #[test]
    fn message_pages_tolerate_empty_and_bare_arrays() {
        assert_eq!(decode_messages(b"").unwrap(), MessagePage::default());
        assert_eq!(
            decode_messages(br#"{"data":null,"cursor":null}"#).unwrap(),
            MessagePage::default()
        );
        let bare = decode_messages(br#"[{"id":"msg_1","content":[]}]"#).unwrap();
        assert_eq!(bare.messages.len(), 1);
        assert_eq!(bare.next, None);
        assert!(decode_messages(br#"{"data":{}}"#).is_err());
    }
}
