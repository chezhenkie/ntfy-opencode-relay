//! Native permission replies.
//!
//! `/allow`, `/always`, and `/deny` resolve a registry record: the ask that was
//! pushed to the phone, bound to its session and request id at publish time.
//! The pin, the routing preference, the active map, and session recency are
//! never read here; the signature cannot reach them.
//!
//! Server outcomes follow spec 09 section 6: a missing or already answered ask
//! removes the record and acks truthfully, a transient failure keeps it.

use std::fmt;

use ntfy_relay_core::adapter::{AdapterError, PermissionApi, PermissionDecision};
use ntfy_relay_core::asks::{Ask, AskStore, Resolve};

/// `/allow`, `/always`, and `/deny` map to the three reply labels.
pub fn decision_for_native(verb: &str) -> Option<PermissionDecision> {
    match verb {
        "allow" => Some(PermissionDecision::Once),
        "always" => Some(PermissionDecision::Always),
        "deny" => Some(PermissionDecision::Reject),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionError {
    /// The registry holds no record.
    NoAsk,
    /// A named code is not registered; `available` is newest first.
    UnknownCode {
        requested: String,
        available: Vec<Ask>,
    },
    /// The server rejected the reply for a reason a retry will not fix.
    Server { status: u16, message: String },
    /// Transport failure or a server fault; the record stays.
    Adapter(AdapterError),
}

impl PermissionError {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Server { status, .. } => Some(*status),
            Self::Adapter(error) => error.status(),
            Self::NoAsk | Self::UnknownCode { .. } => None,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::NoAsk => "no pending permission ask".to_string(),
            Self::UnknownCode {
                requested,
                available,
            } => {
                let listed: Vec<String> = available
                    .iter()
                    .take(3)
                    .map(|ask| format!("{} ({})", ask.code, ask.title))
                    .collect();
                if listed.is_empty() {
                    format!("unknown permission code {requested}")
                } else {
                    format!(
                        "unknown permission code {requested}; pending: {}",
                        listed.join(", ")
                    )
                }
            }
            Self::Server { message, .. } => message.clone(),
            Self::Adapter(error) => error.message().to_string(),
        }
    }
}

impl fmt::Display for PermissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for PermissionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    Answered {
        title: String,
        code: String,
        decision: PermissionDecision,
        remaining: usize,
    },
    AlreadyAnswered {
        title: String,
        code: String,
    },
}

impl PermissionOutcome {
    pub fn code(&self) -> &str {
        match self {
            Self::Answered { code, .. } | Self::AlreadyAnswered { code, .. } => code,
        }
    }

    /// `approved once in <title> (<code>), N more pending`, or the matching
    /// decision or already-answered wording.
    pub fn ack_body(&self) -> String {
        match self {
            Self::Answered {
                title,
                code,
                decision,
                remaining,
            } => {
                let verb = match decision {
                    PermissionDecision::Once => "approved once",
                    PermissionDecision::Always => "approved always",
                    PermissionDecision::Reject => "rejected",
                };
                if *remaining > 0 {
                    format!("{verb} in {title} ({code}), {remaining} more pending")
                } else {
                    format!("{verb} in {title} ({code})")
                }
            }
            Self::AlreadyAnswered { title, code } => {
                format!("already answered in {title} ({code})")
            }
        }
    }
}

/// Resolves one registry record and posts the decision to its own session and
/// request id. A successful or already-answered reply removes the record.
pub async fn reply_to_permission(
    permissions: &dyn PermissionApi,
    asks: &AskStore,
    decision: PermissionDecision,
    code: Option<&str>,
) -> Result<PermissionOutcome, PermissionError> {
    let ask = match asks.resolve(code) {
        Resolve::Empty => return Err(PermissionError::NoAsk),
        Resolve::Unknown {
            requested,
            available,
        } => {
            return Err(PermissionError::UnknownCode {
                requested,
                available,
            })
        }
        Resolve::AlreadyAnswered(ask) => {
            return Ok(PermissionOutcome::AlreadyAnswered {
                title: ask.title,
                code: ask.code,
            })
        }
        Resolve::Found(ask) => ask,
    };

    match permissions
        .reply_permission(&ask.session, &ask.request, decision)
        .await
    {
        Ok(()) => {
            asks.remove(&ask.request);
            Ok(PermissionOutcome::Answered {
                title: ask.title,
                code: ask.code,
                decision,
                remaining: asks.count(),
            })
        }
        Err(error) => match error.status() {
            Some(404) | Some(409) => {
                asks.remove(&ask.request);
                Ok(PermissionOutcome::AlreadyAnswered {
                    title: ask.title,
                    code: ask.code,
                })
            }
            Some(400) if already_answered(&error) => {
                asks.remove(&ask.request);
                Ok(PermissionOutcome::AlreadyAnswered {
                    title: ask.title,
                    code: ask.code,
                })
            }
            Some(status @ 400..=499) => Err(PermissionError::Server {
                status,
                message: error.message().to_string(),
            }),
            _ => Err(PermissionError::Adapter(error)),
        },
    }
}

fn already_answered(error: &AdapterError) -> bool {
    let text = error.message().to_ascii_lowercase();
    text.contains("already answered") || text.contains("replied")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_verbs_map_to_the_three_decisions() {
        assert_eq!(decision_for_native("allow"), Some(PermissionDecision::Once));
        assert_eq!(
            decision_for_native("always"),
            Some(PermissionDecision::Always)
        );
        assert_eq!(
            decision_for_native("deny"),
            Some(PermissionDecision::Reject)
        );
        assert_eq!(decision_for_native("steer"), None);
    }

    #[test]
    fn ack_names_the_title_the_code_and_the_remaining_count() {
        let outcome = PermissionOutcome::Answered {
            title: "Fix links".to_string(),
            code: "3f2a".to_string(),
            decision: PermissionDecision::Once,
            remaining: 2,
        };
        assert_eq!(
            outcome.ack_body(),
            "approved once in Fix links (3f2a), 2 more pending"
        );
        let outcome = PermissionOutcome::Answered {
            title: "Fix links".to_string(),
            code: "3f2a".to_string(),
            decision: PermissionDecision::Reject,
            remaining: 0,
        };
        assert_eq!(outcome.ack_body(), "rejected in Fix links (3f2a)");
        let outcome = PermissionOutcome::AlreadyAnswered {
            title: "Fix links".to_string(),
            code: "3f2a".to_string(),
        };
        assert_eq!(outcome.ack_body(), "already answered in Fix links (3f2a)");
    }

    #[test]
    fn an_empty_registry_never_names_a_session() {
        assert_eq!(
            PermissionError::NoAsk.message(),
            "no pending permission ask"
        );
    }

    #[test]
    fn an_unknown_code_lists_at_most_three_records() {
        let ask = |index: usize| Ask {
            code: format!("c{index:03}"),
            request: format!("per_{index}"),
            session: "ses_1".to_string(),
            title: format!("T{index}"),
            pushed: index as i64,
        };
        let error = PermissionError::UnknownCode {
            requested: "ffff".to_string(),
            available: vec![ask(3), ask(2), ask(1), ask(0)],
        };
        let message = error.message();
        assert!(message.starts_with("unknown permission code ffff; pending: "));
        assert!(message.contains("c003 (T3)"));
        assert!(!message.contains("c000"));
        assert!(!message.contains("ses_"));
    }
}
