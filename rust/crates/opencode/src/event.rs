//! Native event stream types for `GET /api/event`.
//!
//! Mirrors `parseRuntimeEvent` in the plugin's `dist/event-adapter.js`, the
//! production parser. The stream carries envelopes
//! `{ id, created?, type, data, location?, durable? }`; only the routes
//! `outbound` maps are surfaced here, so deltas, reasoning, and tool traffic
//! never reach notification code. Question asks arrive as `form.created` with
//! `metadata.kind == "question"` (captured 2026-11-11), not as a `question.*`
//! frame; the `form.*` route is parsed for that mapping.
//!
//! Envelope and payload field names come from the server's `openapi.json`
//! (`V2Event.*` and `SessionStatus`) and from the captured stream under
//! `tests/fixtures`. Required fields are strict on purpose: when an OpenCode
//! upgrade renames one, the event is dropped and the contract test fails.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub id: String,
    pub kind: String,
    pub created: Option<i64>,
    /// `location.directory`; absent on frames the server emits without it.
    pub location: Option<String>,
    pub data: Value,
}

impl Envelope {
    pub fn parse(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let id = non_empty_opt(object.get("id"))?.to_string();
        let kind = non_empty_opt(object.get("type"))?.to_string();
        let created = object.get("created").and_then(integer);
        let location = object
            .get("location")
            .and_then(Value::as_object)
            .and_then(|location| location.get("directory"))
            .and_then(non_empty)
            .map(str::to_string);
        let data = object.get("data").cloned().unwrap_or(Value::Null);
        Some(Self {
            id,
            kind,
            created,
            location,
            data,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    Add,
    Change,
    Unlink,
}

impl FileAction {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "add" => Some(Self::Add),
            "change" => Some(Self::Change),
            "unlink" => Some(Self::Unlink),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Change => "change",
            Self::Unlink => "unlink",
        }
    }
}

/// One field of a question form, narrowed to what a phone push renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormField {
    pub title: String,
    pub description: Option<String>,
    /// `string`, `multiselect`, `boolean`, `integer`, `number`, or `external`.
    pub kind: String,
    pub hidden: bool,
    pub options: Vec<FormOption>,
}

impl FormField {
    pub fn multiple(&self) -> bool {
        self.kind == "multiselect"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormOption {
    pub label: String,
    pub description: Option<String>,
}

/// The routes `outbound` maps to pushes. `SessionIdle` covers both
/// `session.status` with `status.type == "idle"` and `session.execution.succeeded`,
/// which is how the patched plugin avoids a duplicate per run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    SessionCreated {
        session_id: String,
        parent_id: Option<String>,
    },
    SessionDeleted {
        session_id: String,
    },
    SessionIdle {
        session_id: String,
    },
    SessionError {
        session_id: String,
        message: String,
    },
    TurnStarted {
        session_id: String,
    },
    TextEnded {
        session_id: String,
        assistant_message_id: String,
        ordinal: i64,
        text: String,
        time: i64,
    },
    Interrupted {
        session_id: String,
        reason: Option<String>,
    },
    FileChanged {
        action: FileAction,
        path: String,
    },
    PermissionAsked {
        session_id: Option<String>,
        request_id: Option<String>,
        permission: String,
        patterns: Vec<String>,
    },
    PermissionReplied {
        request_id: String,
    },
    FormCreated {
        session_id: String,
        form_id: String,
        title: String,
        /// `metadata.kind`; `Some("question")` marks the built-in question tool.
        kind: Option<String>,
        fields: Vec<FormField>,
    },
}

/// Parses one SSE frame payload.
pub fn parse(value: &Value) -> Option<RuntimeEvent> {
    let envelope = Envelope::parse(value)?;
    parse_envelope(&envelope)
}

pub fn parse_envelope(envelope: &Envelope) -> Option<RuntimeEvent> {
    let kind = envelope.kind.as_str();
    // `question.*` frames are not emitted by this build; `form.created` is the
    // real ask path. Other form frames carry no phone-facing payload.
    if kind.starts_with("question.") {
        return None;
    }
    let data = envelope.data.as_object();
    match kind {
        // Native `session.created` is flat: the id and optional parent live on
        // `data`, not in a nested `info`.
        "session.created" => Some(RuntimeEvent::SessionCreated {
            session_id: text_field(data, "sessionID")?.to_string(),
            parent_id: text_field(data, "parentID").map(str::to_string),
        }),
        "session.deleted" => Some(RuntimeEvent::SessionDeleted {
            session_id: text_field(data, "sessionID")?.to_string(),
        }),
        "session.status" => {
            let session_id = text_field(data, "sessionID")?;
            let status = data
                .and_then(|data| data.get("status"))
                .and_then(Value::as_object)?;
            if text_field(Some(status), "type") != Some("idle") {
                return None;
            }
            Some(RuntimeEvent::SessionIdle {
                session_id: session_id.to_string(),
            })
        }
        // A completed run emits execution.succeeded, not a status idle frame.
        "session.execution.succeeded" => Some(RuntimeEvent::SessionIdle {
            session_id: text_field(data, "sessionID")?.to_string(),
        }),
        "session.execution.failed" => Some(RuntimeEvent::SessionError {
            session_id: text_field(data, "sessionID")?.to_string(),
            message: extract_error_message(data.and_then(|data| data.get("error"))),
        }),
        "session.execution.started" => Some(RuntimeEvent::TurnStarted {
            session_id: text_field(data, "sessionID")?.to_string(),
        }),
        "session.text.ended" => {
            let session_id = text_field(data, "sessionID")?;
            let text = data
                .and_then(|data| data.get("text"))
                .and_then(Value::as_str)?;
            if text.trim().is_empty() {
                return None;
            }
            Some(RuntimeEvent::TextEnded {
                session_id: session_id.to_string(),
                assistant_message_id: text_field(data, "assistantMessageID")
                    .unwrap_or("unknown")
                    .to_string(),
                ordinal: data
                    .and_then(|data| data.get("ordinal"))
                    .and_then(integer)
                    .unwrap_or(0),
                text: text.to_string(),
                time: envelope.created.unwrap_or(0),
            })
        }
        "session.execution.interrupted" => Some(RuntimeEvent::Interrupted {
            session_id: text_field(data, "sessionID")?.to_string(),
            reason: text_field(data, "reason").map(str::to_string),
        }),
        "filesystem.changed" => Some(RuntimeEvent::FileChanged {
            action: FileAction::parse(text_field(data, "event")?)?,
            path: text_field(data, "file")?.to_string(),
        }),
        "permission.asked" => {
            let patterns = data
                .and_then(|data| data.get("resources"))
                .and_then(Value::as_array)
                .map(|resources| {
                    resources
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Some(RuntimeEvent::PermissionAsked {
                session_id: text_field(data, "sessionID").map(str::to_string),
                request_id: text_field(data, "id").map(str::to_string),
                permission: text_field(data, "action").unwrap_or("unknown").to_string(),
                patterns,
            })
        }
        "permission.replied" => Some(RuntimeEvent::PermissionReplied {
            request_id: text_field(data, "requestID")?.to_string(),
        }),
        "form.created" => {
            let form = data
                .and_then(|data| data.get("form"))
                .and_then(Value::as_object)?;
            let fields = form
                .get("fields")
                .and_then(Value::as_array)
                .map(|items| parse_form_fields(items))
                .unwrap_or_default();
            Some(RuntimeEvent::FormCreated {
                session_id: text_field(Some(form), "sessionID")?.to_string(),
                form_id: text_field(Some(form), "id")?.to_string(),
                title: form
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                kind: form
                    .get("metadata")
                    .and_then(Value::as_object)
                    .and_then(|metadata| text_field(Some(metadata), "kind"))
                    .map(str::to_string),
                fields,
            })
        }
        _ => None,
    }
}

/// Error text safe to publish: the type or name plus the first message-like
/// field, never a stack trace or an HTTP response body.
pub fn extract_error_message(error: Option<&Value>) -> String {
    let Some(error) = error else {
        return "unknown error".to_string();
    };
    if let Some(text) = error.as_str() {
        if !text.trim().is_empty() {
            return text.to_string();
        }
    }
    let Some(record) = error.as_object() else {
        return "unknown error".to_string();
    };
    let label = record
        .get("type")
        .and_then(non_empty)
        .or_else(|| record.get("name").and_then(non_empty))
        .unwrap_or("error");
    let message = record
        .get("message")
        .and_then(non_empty)
        .or_else(|| {
            record
                .get("data")
                .and_then(Value::as_object)
                .and_then(|data| data.get("message"))
                .and_then(non_empty)
        })
        .unwrap_or("no additional details");
    format!("{label}: {message}")
}

fn parse_form_fields(items: &[Value]) -> Vec<FormField> {
    items
        .iter()
        .filter_map(|item| {
            let field = item.as_object()?;
            let options = field
                .get("options")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(parse_form_option).collect())
                .unwrap_or_default();
            Some(FormField {
                title: form_text(field, "title"),
                description: form_optional(field, "description"),
                kind: text_field(Some(field), "type")?.to_string(),
                hidden: field
                    .get("hidden")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                options,
            })
        })
        .collect()
}

fn parse_form_option(value: &Value) -> Option<FormOption> {
    let option = value.as_object()?;
    Some(FormOption {
        label: form_text(option, "label"),
        description: form_optional(option, "description"),
    })
}

fn form_text(field: &serde_json::Map<String, Value>, key: &str) -> String {
    field
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn form_optional(field: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    field
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn non_empty(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

fn non_empty_opt(value: Option<&Value>) -> Option<&str> {
    value.and_then(non_empty)
}

fn text_field<'a>(data: Option<&'a serde_json::Map<String, Value>>, key: &str) -> Option<&'a str> {
    non_empty(data?.get(key)?)
}

/// JSON numbers arrive as integers; a float with no fraction still counts, like
/// `Number.isInteger` in the Node parser.
fn integer(value: &Value) -> Option<i64> {
    let number = value.as_number()?;
    if let Some(integer) = number.as_i64() {
        return Some(integer);
    }
    let float = number.as_f64()?;
    if float.fract() == 0.0 && float >= i64::MIN as f64 && float <= i64::MAX as f64 {
        Some(float as i64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope(kind: &str, data: Value) -> Value {
        json!({
            "id": "evt_1",
            "created": 1794353009570i64,
            "type": kind,
            "location": { "directory": "C:\\work" },
            "data": data
        })
    }

    #[test]
    fn envelope_carries_the_flat_header_fields() {
        let value = envelope("session.created", json!({"sessionID": "ses_a"}));
        let envelope = Envelope::parse(&value).unwrap();
        assert_eq!(envelope.id, "evt_1");
        assert_eq!(envelope.kind, "session.created");
        assert_eq!(envelope.created, Some(1794353009570));
        assert_eq!(envelope.location.as_deref(), Some("C:\\work"));
    }

    #[test]
    fn created_session_keeps_the_optional_parent() {
        assert_eq!(
            parse(&envelope(
                "session.created",
                json!({"sessionID": "ses_a", "parentID": "ses_root", "slug": "x"})
            )),
            Some(RuntimeEvent::SessionCreated {
                session_id: "ses_a".to_string(),
                parent_id: Some("ses_root".to_string()),
            })
        );
        assert_eq!(
            parse(&envelope("session.created", json!({"sessionID": "ses_a"}))),
            Some(RuntimeEvent::SessionCreated {
                session_id: "ses_a".to_string(),
                parent_id: None,
            })
        );
    }

    #[test]
    fn status_only_surfaces_idle() {
        let idle = envelope(
            "session.status",
            json!({"sessionID": "ses_a", "status": {"type": "idle"}}),
        );
        assert_eq!(
            parse(&idle),
            Some(RuntimeEvent::SessionIdle {
                session_id: "ses_a".to_string()
            })
        );
        let busy = envelope(
            "session.status",
            json!({"sessionID": "ses_a", "status": {"type": "busy"}}),
        );
        assert_eq!(parse(&busy), None);
        let retry = envelope(
            "session.status",
            json!({"sessionID": "ses_a", "status": {"type": "retry", "attempt": 1, "message": "m", "next": 2}}),
        );
        assert_eq!(parse(&retry), None);
    }

    #[test]
    fn text_ended_requires_text_and_defaults_the_ordinal() {
        let event = parse(&envelope(
            "session.text.ended",
            json!({"sessionID": "ses_a", "assistantMessageID": "msg_1", "text": " done \n"}),
        ));
        assert_eq!(
            event,
            Some(RuntimeEvent::TextEnded {
                session_id: "ses_a".to_string(),
                assistant_message_id: "msg_1".to_string(),
                ordinal: 0,
                text: " done \n".to_string(),
                time: 1794353009570,
            })
        );
        let blank = envelope(
            "session.text.ended",
            json!({"sessionID": "ses_a", "assistantMessageID": "msg_1", "text": "  "}),
        );
        assert_eq!(parse(&blank), None);
    }

    #[test]
    fn failure_uses_the_structured_error_text() {
        let event = parse(&envelope(
            "session.execution.failed",
            json!({"sessionID": "ses_a", "error": {"type": "ProviderError", "message": "rate limited", "status": 429}}),
        ));
        assert_eq!(
            event,
            Some(RuntimeEvent::SessionError {
                session_id: "ses_a".to_string(),
                message: "ProviderError: rate limited".to_string(),
            })
        );
    }

    #[test]
    fn permission_events_keep_optional_ids() {
        let event = parse(&envelope(
            "permission.asked",
            json!({"id": "per_1", "sessionID": "ses_a", "action": "bash", "resources": ["git status", 7]}),
        ));
        assert_eq!(
            event,
            Some(RuntimeEvent::PermissionAsked {
                session_id: Some("ses_a".to_string()),
                request_id: Some("per_1".to_string()),
                permission: "bash".to_string(),
                patterns: vec!["git status".to_string()],
            })
        );
        let without_session = envelope(
            "permission.asked",
            json!({"id": "per_1", "action": "bash", "resources": []}),
        );
        assert!(matches!(
            parse(&without_session),
            Some(RuntimeEvent::PermissionAsked {
                session_id: None,
                ..
            })
        ));
    }

    #[test]
    fn question_and_delta_routes_are_dropped() {
        for kind in [
            "question.asked",
            "question.v2.created",
            "form.replied",
            "form.cancelled",
            "session.text.delta",
            "session.reasoning.ended",
            "server.connected",
        ] {
            assert_eq!(parse(&envelope(kind, json!({"sessionID": "ses_a"}))), None);
        }
    }

    #[test]
    fn form_created_reads_a_question_payload() {
        let value: Value =
            serde_json::from_str(include_str!("../tests/fixtures/form-created-question.json"))
                .unwrap();
        let event = parse(&value).unwrap();
        let RuntimeEvent::FormCreated {
            session_id,
            form_id,
            title,
            kind,
            fields,
        } = event
        else {
            panic!("expected a form event");
        };
        assert_eq!(session_id, "ses_e3486e372ffeUCqVQtxzOv1AXd");
        assert_eq!(form_id, "frm_1cb7e1b84001U2uZNIs350NgKa");
        assert_eq!(title, "Example question");
        assert_eq!(kind.as_deref(), Some("question"));
        assert_eq!(fields.len(), 1);
        let field = &fields[0];
        assert_eq!(field.title, "Scope choice");
        assert!(field
            .description
            .as_deref()
            .unwrap()
            .starts_with("Pick one option"));
        assert_eq!(field.kind, "string");
        assert!(!field.hidden);
        assert!(!field.multiple());
        assert_eq!(field.options.len(), 2);
        assert_eq!(field.options[0].label, "Option one");
        assert!(field.options[0]
            .description
            .as_deref()
            .unwrap()
            .contains("First example"));
    }

    #[test]
    fn a_form_without_a_session_is_dropped() {
        let value = envelope(
            "form.created",
            json!({"form": {"id": "frm_1", "title": "x", "fields": []}}),
        );
        assert_eq!(parse(&value), None);
    }
}
