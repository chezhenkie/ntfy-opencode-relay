//! `SessionApi` over the local OpenCode HTTP API.
//!
//! Mirrors `createOpenCodeAdapter` in `ntfy-opencode-adapter.mjs`:
//!
//! - `session.create` exists only when both `adapter.agent` and
//!   `adapter.model` are configured.
//! - Created titles are cleaned, whitespace-collapsed, and capped at 200 UTF-16
//!   units, then trimmed again (JavaScript `slice(0, 200).trim()`).
//! - Sessions merge across every project row of an entry and sort by
//!   `time.updated` descending; Node inserts into a `Map` and sorts stably.
//! - Listing marks a session running from `/api/session/active`.
//! - `session_signals` carries `time.viewed` and `parentID` to the resolver.
//!   The adapter owns no routing state.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ntfy_relay_core::adapter::{
    AdapterError, Capability, Delivery, FormApi, NewSession, Permission, PermissionApi,
    PermissionDecision, ProjectEntry, PromptFile, PromptRequest, Session, SessionApi,
    SessionSignal,
};

use crate::api::{
    Api, ApiError, CreateSession, ModelRef, PermissionInfo, ReplyDecision, SessionInfo,
};

pub const MAX_CREATED_TITLE_CHARS: usize = 200;
/// `withRetry`: two retries, 1 s then 2 s, for network errors and 5xx only.
pub const PROMPT_RETRIES: usize = 2;
pub const PROMPT_RETRY_DELAYS_MS: [u64; PROMPT_RETRIES] = [1000, 2000];

pub struct OpenCodeAdapter {
    api: Arc<dyn Api>,
    agent: Option<String>,
    model: Option<ModelRef>,
    capabilities: Vec<Capability>,
}

impl OpenCodeAdapter {
    pub fn new(api: Arc<dyn Api>, agent: Option<&str>, model: Option<ModelRef>) -> Self {
        let agent = agent
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let mut capabilities = vec![
            Capability::SessionList,
            Capability::SessionPin,
            Capability::SessionAuto,
            Capability::Prompt,
        ];
        if agent.is_some() && model.is_some() {
            capabilities.push(Capability::SessionCreate);
        }
        Self {
            api,
            agent,
            model,
            capabilities,
        }
    }

    /// Sessions for a project entry, deduplicated by id across project rows and
    /// sorted by `time.updated` descending.
    pub async fn sessions(
        &self,
        entry: &ProjectEntry,
        limit: usize,
    ) -> Result<Vec<SessionInfo>, AdapterError> {
        let mut merged: Vec<SessionInfo> = Vec::new();
        let mut positions: HashMap<String, usize> = HashMap::new();
        for project_id in &entry.project_ids {
            for session in self
                .api
                .sessions(project_id, limit)
                .await
                .map_err(adapter_error)?
            {
                match positions.get(&session.id) {
                    Some(&index) => merged[index] = session,
                    None => {
                        positions.insert(session.id.clone(), merged.len());
                        merged.push(session);
                    }
                }
            }
        }
        merged.sort_by_key(|session| std::cmp::Reverse(session.time.updated));
        Ok(merged)
    }

    /// Fetch one session by exact id. A missing session is `Ok(None)`.
    pub async fn get_session(&self, session_id: &str) -> Result<Option<Session>, AdapterError> {
        match self.api.session(session_id).await {
            Ok(info) => Ok(Some(to_session(info, false))),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(adapter_error(error)),
        }
    }
}

#[async_trait]
impl SessionApi for OpenCodeAdapter {
    fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    async fn create_session(&self, request: NewSession<'_>) -> Result<Session, AdapterError> {
        let (Some(agent), Some(model)) = (self.agent.as_deref(), self.model.as_ref()) else {
            return Err(AdapterError::new(
                "session.create needs adapter.agent and adapter.model",
            ));
        };
        let title = clean_title(request.title);
        let directory = request.entry.canonical.to_string_lossy();
        let info = self
            .api
            .create_session(CreateSession {
                directory: &directory,
                agent,
                model,
                title: title.as_deref(),
            })
            .await
            .map_err(adapter_error)?;
        if info.id.is_empty() {
            return Err(AdapterError::new("session create returned no id"));
        }
        Ok(Session {
            id: info.id,
            title: info.title,
            running: false,
            updated: info.time.updated,
            project_id: info.project_id,
        })
    }

    async fn list_sessions(
        &self,
        entry: &ProjectEntry,
        limit: usize,
    ) -> Result<Vec<Session>, AdapterError> {
        Ok(self
            .session_signals(entry, limit)
            .await?
            .into_iter()
            .map(|signal| signal.session)
            .collect())
    }

    async fn session_signals(
        &self,
        entry: &ProjectEntry,
        limit: usize,
    ) -> Result<Vec<SessionSignal>, AdapterError> {
        let sessions = self.sessions(entry, limit).await?;
        let running: HashSet<String> = self
            .api
            .active_ids()
            .await
            .map_err(adapter_error)?
            .into_iter()
            .collect();
        Ok(sessions
            .into_iter()
            .map(|info| SessionSignal {
                session: Session {
                    running: running.contains(&info.id),
                    id: info.id,
                    title: info.title,
                    updated: info.time.updated,
                    project_id: info.project_id,
                },
                viewed: info.time.viewed,
                parent_id: info.parent_id,
            })
            .collect())
    }

    async fn prompt(&self, request: PromptRequest<'_>) -> Result<(), AdapterError> {
        let files = prompt_files(request.files)?;
        let text = prompt_text(request.text, request.files);
        let delivery = match request.delivery {
            Delivery::Queue => crate::api::Delivery::Queue,
            Delivery::Steer => crate::api::Delivery::Steer,
        };
        let mut attempt = 0;
        loop {
            match self
                .api
                .prompt(request.session_id, &text, delivery, &files)
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) => {
                    attempt += 1;
                    let retriable = match error.code() {
                        None => true,
                        Some(status) => status >= 500,
                    };
                    if !retriable || attempt > PROMPT_RETRIES {
                        return Err(adapter_error(error));
                    }
                    let delay = PROMPT_RETRY_DELAYS_MS[attempt - 1];
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }
}

fn to_session(info: SessionInfo, running: bool) -> Session {
    Session {
        id: info.id,
        title: info.title,
        running,
        updated: info.time.updated,
        project_id: info.project_id,
    }
}

/// `pathToFileURL` plus the relay's attached-file trailer.
fn prompt_files(files: &[PromptFile]) -> Result<Vec<crate::api::PromptFile>, AdapterError> {
    files
        .iter()
        .map(|file| {
            let uri = url::Url::from_file_path(&file.path).map_err(|_| {
                AdapterError::new(format!(
                    "cannot build a file URL for {}",
                    file.path.display()
                ))
            })?;
            Ok(crate::api::PromptFile {
                uri: uri.to_string(),
                name: file.name.clone(),
            })
        })
        .collect()
}

fn prompt_text(text: &str, files: &[PromptFile]) -> String {
    let attached = files
        .iter()
        .map(|file| format!("Attached file: {}", file.path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let joined = [text, attached.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if joined.is_empty() {
        "Please inspect the attached file.".to_string()
    } else {
        joined
    }
}

fn permission_decision(decision: PermissionDecision) -> ReplyDecision {
    match decision {
        PermissionDecision::Once => ReplyDecision::Once,
        PermissionDecision::Always => ReplyDecision::Always,
        PermissionDecision::Reject => ReplyDecision::Reject,
    }
}

fn permission(info: PermissionInfo) -> Permission {
    Permission { id: info.id }
}

#[async_trait]
impl PermissionApi for OpenCodeAdapter {
    async fn list_permissions(&self, session_id: &str) -> Result<Vec<Permission>, AdapterError> {
        let requests = self
            .api
            .permissions(session_id)
            .await
            .map_err(adapter_error)?;
        Ok(requests.into_iter().map(permission).collect())
    }

    async fn reply_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), AdapterError> {
        self.api
            .reply_permission(session_id, permission_id, permission_decision(decision))
            .await
            .map_err(adapter_error)
    }
}

#[async_trait]
impl FormApi for OpenCodeAdapter {
    async fn cancel_form(
        &self,
        session_id: &str,
        form_id: &str,
        message: &str,
    ) -> Result<(), AdapterError> {
        self.api
            .cancel_form(session_id, form_id, message)
            .await
            .map_err(adapter_error)
    }
}

fn adapter_error(error: ApiError) -> AdapterError {
    let status = error.code();
    let message = error.message().to_string();
    match status {
        Some(status) => AdapterError::with_status(status, message),
        None => AdapterError::new(message),
    }
}

/// `cleanTitle`: control characters become spaces, JavaScript whitespace runs
/// collapse to one space, the ends are trimmed, and the result is capped at
/// 200 UTF-16 units and trimmed again. An empty result is `None`.
pub fn clean_title(value: Option<&str>) -> Option<String> {
    let value = value?;
    let mut text = String::with_capacity(value.len());
    let mut pending_space = false;
    for ch in value.chars() {
        if is_js_space(ch) || ch == '\u{7f}' || (ch as u32) < 0x20 {
            if !text.is_empty() {
                pending_space = true;
            }
        } else {
            if pending_space {
                text.push(' ');
                pending_space = false;
            }
            text.push(ch);
        }
    }
    if text.is_empty() {
        return None;
    }
    if text.encode_utf16().count() > MAX_CREATED_TITLE_CHARS {
        let mut used = 0;
        let mut end = text.len();
        for (index, ch) in text.char_indices() {
            let size = ch.len_utf16();
            if used + size > MAX_CREATED_TITLE_CHARS {
                end = index;
                break;
            }
            used += size;
        }
        text.truncate(end);
        while text.ends_with(is_js_space) {
            text.pop();
        }
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// ECMAScript `\s`. Rust's `char::is_whitespace` differs at U+0085 and U+FEFF.
fn is_js_space(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\u{09}'..='\u{0d}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_collapse_control_and_space_runs() {
        assert_eq!(
            clean_title(Some("  prune\tcheck\n  now  ")).as_deref(),
            Some("prune check now")
        );
        assert_eq!(clean_title(Some("\u{0}\u{1}\u{7f}")), None);
        assert_eq!(clean_title(Some("   ")), None);
        assert_eq!(clean_title(None), None);
    }

    #[test]
    fn titles_keep_plain_text_and_inner_punctuation() {
        assert_eq!(
            clean_title(Some("fix: /new - panic")).as_deref(),
            Some("fix: /new - panic")
        );
    }

    #[test]
    fn cap_is_200_utf16_units_then_trimmed() {
        let exact = "a".repeat(200);
        assert_eq!(clean_title(Some(&exact)).as_deref(), Some(exact.as_str()));

        let over = format!("{}   tail", "a".repeat(200));
        assert_eq!(clean_title(Some(&over)).as_deref(), Some(exact.as_str()));

        // Each astral character is two UTF-16 units, so 100 fit the cap.
        let astral = "\u{1f600}".repeat(120);
        let cleaned = clean_title(Some(&astral)).unwrap();
        assert_eq!(cleaned, "\u{1f600}".repeat(100));

        // A cut that lands after a space trims the trailing space.
        let spaced = format!("{} b", "a".repeat(199));
        assert_eq!(
            clean_title(Some(&spaced)).as_deref(),
            Some("a".repeat(199).as_str())
        );
    }
}
