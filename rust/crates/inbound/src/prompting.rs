//! Prompt delivery: resolve a target, send text and saved files, and describe
//! the ack. The adapter owns the wire body and retries; inbound owns the
//! routing order and the user-visible outcome.

use std::fmt;

use ntfy_relay_core::adapter::{
    AdapterError, Delivery, ProjectEntry, PromptFile, PromptRequest, Session, SessionApi,
};

use crate::routing::{resolve, Hints, Resolved, Windows, ROUTE_SESSION_LIMIT};
use crate::session::session_title;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliverError {
    NoSession,
    Adapter(AdapterError),
}

impl DeliverError {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Adapter(error) => error.status(),
            Self::NoSession => None,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::NoSession => "no session in project; relay never creates one".to_string(),
            Self::Adapter(error) => error.message().to_string(),
        }
    }
}

impl fmt::Display for DeliverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for DeliverError {}

/// One prompt to deliver: the entry, the routing hints, and the body.
pub struct DeliveryRequest<'a> {
    pub entry: &'a ProjectEntry,
    pub hints: Hints<'a>,
    pub prefer_running: bool,
    pub windows: Windows,
    pub now: i64,
    pub text: &'a str,
    pub delivery: Delivery,
    pub files: &'a [PromptFile],
}

/// Send one prompt to the resolved session. Free text never creates a session.
/// The resolution reads the live snapshot; the caller persists the returned
/// target after a successful prompt.
pub async fn deliver_prompt(
    adapter: &dyn SessionApi,
    request: DeliveryRequest<'_>,
) -> Result<Resolved, DeliverError> {
    let signals = adapter
        .session_signals(request.entry, ROUTE_SESSION_LIMIT)
        .await
        .map_err(DeliverError::Adapter)?;
    let resolved = resolve(
        &signals,
        request.hints,
        request.now,
        request.prefer_running,
        request.windows,
    )
    .ok_or(DeliverError::NoSession)?;
    adapter
        .prompt(PromptRequest {
            session_id: &resolved.session.id,
            text: request.text,
            delivery: request.delivery,
            files: request.files,
        })
        .await
        .map_err(DeliverError::Adapter)?;
    Ok(resolved)
}

/// Ack suffix and body for a delivered prompt, matching the Node relay.
pub fn delivery_ack(session: &Session, delivery: Delivery) -> (&'static str, String) {
    let suffix = match delivery {
        Delivery::Queue => "queued",
        Delivery::Steer => "steering",
    };
    let verb = match delivery {
        Delivery::Queue => "queued to",
        Delivery::Steer => "steering",
    };
    let title = session_title(&session.id, session.title.as_deref());
    (suffix, format!("{verb} {title}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_text_names_the_delivery_and_target() {
        let session = Session {
            id: "ses_1".to_string(),
            title: Some("Fix links".to_string()),
            ..Session::default()
        };
        assert_eq!(
            delivery_ack(&session, Delivery::Queue),
            ("queued", "queued to Fix links".to_string())
        );
        assert_eq!(
            delivery_ack(&session, Delivery::Steer),
            ("steering", "steering Fix links".to_string())
        );
    }

    #[test]
    fn no_session_has_the_exact_error_text() {
        let error = DeliverError::NoSession;
        assert_eq!(
            error.message(),
            "no session in project; relay never creates one"
        );
        assert_eq!(error.status(), None);
    }
}
