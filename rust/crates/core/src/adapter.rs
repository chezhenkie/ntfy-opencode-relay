//! Harness seam: the session operations the control engine needs.
//!
//! `inbound` never depends on `opencode`. The app injects an implementation
//! through this trait, which keeps the pi and omp adapters beside OpenCode
//! (docs/specsheet `06-rust-crate-split.md`).
//!
//! `async-trait` is a build-time proc macro only; it is not linked into the
//! binary. Boxed futures are what make the trait object-safe for `&dyn`.

use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    SessionCreate,
    SessionList,
    SessionPin,
    SessionAuto,
    Prompt,
}

impl Capability {
    pub const ALL: [Capability; 5] = [
        Self::SessionCreate,
        Self::SessionList,
        Self::SessionPin,
        Self::SessionAuto,
        Self::Prompt,
    ];

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "session.create" => Some(Self::SessionCreate),
            "session.list" => Some(Self::SessionList),
            "session.pin" => Some(Self::SessionPin),
            "session.auto" => Some(Self::SessionAuto),
            "prompt" => Some(Self::Prompt),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionCreate => "session.create",
            Self::SessionList => "session.list",
            Self::SessionPin => "session.pin",
            Self::SessionAuto => "session.auto",
            Self::Prompt => "prompt",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntry {
    pub topic: String,
    pub canonical: PathBuf,
    pub project_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub running: bool,
    /// Milliseconds used to rank sessions for routing.
    pub updated: i64,
    /// Owning project row; a target is only valid inside the entry's project IDs.
    pub project_id: Option<String>,
}

/// One routing candidate: the session plus the signals the resolver needs.
/// `viewed` is written by the desktop client when a session is opened or used;
/// API prompts do not change it. A child session is a subagent and is never a
/// delivery target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSignal {
    pub session: Session,
    pub viewed: i64,
    pub parent_id: Option<String>,
}

/// Delivery mode for a prompt: `Queue` waits for the running turn, `Steer`
/// interrupts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Delivery {
    Queue,
    Steer,
}

impl Delivery {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queue" => Some(Self::Queue),
            "steer" => Some(Self::Steer),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Steer => "steer",
        }
    }
}

/// A file to attach to a prompt. The adapter turns the path into a harness
/// URI; inbound only knows the saved inbox path and the display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptFile {
    pub path: PathBuf,
    pub name: String,
}

#[derive(Debug, Clone, Copy)]
pub struct PromptRequest<'a> {
    pub session_id: &'a str,
    pub text: &'a str,
    pub delivery: Delivery,
    pub files: &'a [PromptFile],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permission {
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionDecision {
    Once,
    Always,
    Reject,
}

impl PermissionDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Always => "always",
            Self::Reject => "reject",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NewSession<'a> {
    pub entry: &'a ProjectEntry,
    pub title: Option<&'a str>,
}

/// Writes the routing hint a topic carries between messages. Implemented by
/// the app's state store; the control engine only mutates the hint.
pub trait TargetSink: Send + Sync {
    /// Remember the session that accepted the last prompt.
    fn set_last(&self, topic: &str, session_id: &str, at: i64);
    /// One-shot target set by `/new` or `/to`; `None` clears it.
    fn set_pending(&self, topic: &str, session_id: Option<&str>, at: i64);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterError {
    status: Option<u16>,
    message: String,
}

impl AdapterError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
        }
    }

    pub fn with_status(status: u16, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            message: message.into(),
        }
    }

    pub fn status(&self) -> Option<u16> {
        self.status
    }

    pub fn is_not_found(&self) -> bool {
        self.status == Some(404)
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AdapterError {}

#[async_trait]
pub trait SessionApi: Send + Sync {
    fn capabilities(&self) -> &[Capability];

    async fn create_session(&self, request: NewSession<'_>) -> Result<Session, AdapterError>;

    async fn list_sessions(
        &self,
        entry: &ProjectEntry,
        limit: usize,
    ) -> Result<Vec<Session>, AdapterError>;

    /// Routing candidates with the desktop-view and parent signals. The default
    /// keeps harnesses without those fields working: viewed is zero and nothing
    /// is treated as a child, so routing falls back to `updated`.
    async fn session_signals(
        &self,
        entry: &ProjectEntry,
        limit: usize,
    ) -> Result<Vec<SessionSignal>, AdapterError> {
        Ok(self
            .list_sessions(entry, limit)
            .await?
            .into_iter()
            .map(|session| SessionSignal {
                session,
                ..SessionSignal::default()
            })
            .collect())
    }

    /// Send one prompt. Retries for transient failures belong to the adapter,
    /// because only it knows whether a repeated call is safe.
    async fn prompt(&self, request: PromptRequest<'_>) -> Result<(), AdapterError>;
}

/// Permission replies are native commands, not table actions. They stay a
/// second trait beside `SessionApi` because the adapter contract in
/// `docs/specsheet\02-message-routes.md` names only create, list, signals, and
/// prompt.
#[async_trait]
pub trait PermissionApi: Send + Sync {
    async fn list_permissions(&self, session_id: &str) -> Result<Vec<Permission>, AdapterError>;

    async fn reply_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), AdapterError>;
}

/// Question forms are dismissed on the phone, not mapped to fields. The free
/// text becomes the cancel reason the asker receives.
#[async_trait]
pub trait FormApi: Send + Sync {
    async fn cancel_form(
        &self,
        session_id: &str,
        form_id: &str,
        message: &str,
    ) -> Result<(), AdapterError>;
}
