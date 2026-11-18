//! Push text and tags, carried over from `event-adapter.js`.
//!
//! The plugin appends `oc-out` at the send boundary; these builders include it
//! directly because every outbound push carries the tag.

use ntfy_relay_ntfy::header::Priority;
use ntfy_relay_opencode::event::FormField;

use crate::publish::OutboundMessage;

pub const QUESTION_MAX_BYTES: usize = 4096;
pub const QUESTION_GENERIC: &str = "The assistant is asking for your input but sent an unreadable question payload.\nOpen the conversation in opencode to answer.";
pub const TRUNCATED: &str = "\n(truncated)";

pub fn idle_message(topic: &str, session_id: &str) -> OutboundMessage {
    OutboundMessage {
        kind: "session.idle",
        topic: topic.to_string(),
        title: format!("{topic} - session idle"),
        message: format!(
            "Session {session_id} has finished its run and is idle.\nIt is waiting for your next instruction."
        ),
        tags: vec!["hourglass_done", "oc-out"],
        priority: Priority::Default,
        file: None,
    }
}

pub fn error_message(topic: &str, session_id: Option<&str>, error: &str) -> OutboundMessage {
    let prefix = match session_id {
        Some(session_id) => format!("Session {session_id} reported an error:"),
        None => "A session reported an error:".to_string(),
    };
    OutboundMessage {
        kind: "session.error",
        topic: topic.to_string(),
        title: format!("{topic} - session error"),
        message: format!("{prefix}\n{error}"),
        tags: vec!["warning", "oc-out"],
        priority: Priority::Default,
        file: None,
    }
}

pub fn permission_message(
    topic: &str,
    permission: &str,
    patterns: &[String],
    session_id: Option<&str>,
    code: &str,
) -> OutboundMessage {
    let mut lines = vec![format!("Permission \"{permission}\" is requested.")];
    if let Some(session_id) = session_id {
        lines.push(format!("Session: {session_id}"));
    }
    if !patterns.is_empty() {
        lines.push(format!("Patterns: {}", patterns.join(", ")));
    }
    lines.push(format!(
        "Reply /allow {code}, /always {code}, or /deny {code}; with one ask, /allow is enough."
    ));
    OutboundMessage {
        kind: "permission.asked",
        topic: topic.to_string(),
        title: format!("{topic} - permission requested"),
        message: lines.join("\n"),
        tags: vec!["lock", "oc-out"],
        priority: Priority::Default,
        file: None,
    }
}

/// Formats a question form. `None` when the form is not a question ask; those
/// forms belong to other tools and never reach the phone.
pub fn question_message(
    topic: &str,
    form_title: &str,
    form_kind: Option<&str>,
    fields: &[FormField],
) -> Option<OutboundMessage> {
    if form_kind != Some("question") {
        return None;
    }
    let mut lines = Vec::new();
    if !form_title.is_empty() {
        lines.push(form_title.to_string());
    }
    let mut rendered = 0;
    for field in fields.iter().filter(|field| !field.hidden) {
        rendered += 1;
        let header = if field.title.is_empty() {
            String::new()
        } else {
            format!("{}: ", field.title)
        };
        let question = field.description.as_deref().unwrap_or("");
        lines.push(
            format!("[{rendered}] {header}{question}")
                .trim()
                .to_string(),
        );
        lines.push(format!(
            "    Multiple answers: {}",
            if field.multiple() {
                "allowed"
            } else {
                "not allowed"
            }
        ));
        for option in &field.options {
            let label = if option.label.is_empty() {
                "(unnamed option)"
            } else {
                option.label.as_str()
            };
            match option.description.as_deref() {
                Some(description) if !description.is_empty() => {
                    lines.push(format!("    - {label} ({description})"));
                }
                _ => lines.push(format!("    - {label}")),
            }
        }
    }
    if rendered == 0 {
        lines.push(QUESTION_GENERIC.to_string());
    }
    Some(OutboundMessage {
        kind: "question.asked",
        topic: topic.to_string(),
        title: format!("{topic} - question"),
        message: bound_question_message(&lines.join("\n")),
        tags: vec!["question", "oc-out"],
        priority: Priority::Default,
        file: None,
    })
}

/// Caps a formatted question at the ntfy client budget, keeping whole code
/// points and marking the cut so the phone never sees a silent mid-option cut.
pub fn bound_question_message(value: &str) -> String {
    if value.len() <= QUESTION_MAX_BYTES {
        return value.to_string();
    }
    let budget = QUESTION_MAX_BYTES - TRUNCATED.len();
    let mut used = 0;
    let mut end = value.len();
    for (index, ch) in value.char_indices() {
        let size = ch.len_utf8();
        if used + size > budget {
            end = index;
            break;
        }
        used += size;
    }
    let mut out = String::with_capacity(used + TRUNCATED.len());
    out.push_str(&value[..end]);
    out.push_str(TRUNCATED);
    out
}

/// First `count` code points, matching `Array.from(text).slice(0, count)`.
pub fn first_characters(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntfy_relay_opencode::event::FormOption;

    #[test]
    fn idle_and_error_match_the_plugin_text() {
        let idle = idle_message("proj", "ses_1");
        assert_eq!(idle.title, "proj - session idle");
        assert_eq!(
            idle.message,
            "Session ses_1 has finished its run and is idle.\nIt is waiting for your next instruction."
        );
        assert_eq!(idle.tags, vec!["hourglass_done", "oc-out"]);

        let error = error_message("proj", Some("ses_1"), "ProviderError: boom");
        assert_eq!(error.title, "proj - session error");
        assert_eq!(
            error.message,
            "Session ses_1 reported an error:\nProviderError: boom"
        );
        assert_eq!(error.tags, vec!["warning", "oc-out"]);
        assert_eq!(
            error_message("proj", None, "x").message,
            "A session reported an error:\nx"
        );
    }

    #[test]
    fn permission_lines_follow_the_plugin_order() {
        let message = permission_message(
            "proj",
            "bash",
            &["git status".to_string(), "git diff".to_string()],
            Some("ses_1"),
            "3f2a",
        );
        assert_eq!(message.title, "proj - permission requested");
        assert_eq!(
            message.message,
            "Permission \"bash\" is requested.\nSession: ses_1\nPatterns: git status, git diff\nReply /allow 3f2a, /always 3f2a, or /deny 3f2a; with one ask, /allow is enough."
        );
        assert_eq!(message.tags, vec!["lock", "oc-out"]);

        let bare = permission_message("proj", "bash", &[], None, "3f2a");
        assert_eq!(
            bare.message,
            "Permission \"bash\" is requested.\nReply /allow 3f2a, /always 3f2a, or /deny 3f2a; with one ask, /allow is enough."
        );
    }

    fn question_field() -> FormField {
        FormField {
            title: "Session scope".to_string(),
            description: Some("Which path?".to_string()),
            kind: "string".to_string(),
            hidden: false,
            options: vec![
                FormOption {
                    label: "Outbound only".to_string(),
                    description: Some("No MCP".to_string()),
                },
                FormOption {
                    label: "Outbound plus MCP".to_string(),
                    description: None,
                },
            ],
        }
    }

    #[test]
    fn a_question_form_renders_every_field_and_option() {
        let message =
            question_message("proj", "Questions", Some("question"), &[question_field()]).unwrap();
        assert_eq!(message.title, "proj - question");
        assert_eq!(
            message.message,
            "Questions\n[1] Session scope: Which path?\n    Multiple answers: not allowed\n    - Outbound only (No MCP)\n    - Outbound plus MCP"
        );
        assert_eq!(message.tags, vec!["question", "oc-out"]);
    }

    #[test]
    fn a_multiselect_field_reports_multiple_answers() {
        let mut field = question_field();
        field.kind = "multiselect".to_string();
        let message = question_message("proj", "", Some("question"), &[field]).unwrap();
        assert!(message.message.contains("Multiple answers: allowed"));
        assert!(message
            .message
            .starts_with("[1] Session scope: Which path?"));
    }

    #[test]
    fn non_question_forms_and_empty_fields_use_the_generic_text() {
        assert!(question_message("proj", "", None, &[]).is_none());
        assert!(question_message("proj", "Form", Some("other"), &[question_field()]).is_none());
        let message = question_message("proj", "Questions", Some("question"), &[]).unwrap();
        assert!(message.message.ends_with(QUESTION_GENERIC));

        let mut hidden = question_field();
        hidden.hidden = true;
        let message = question_message("proj", "Questions", Some("question"), &[hidden]).unwrap();
        assert!(message.message.ends_with(QUESTION_GENERIC));
    }

    #[test]
    fn oversized_questions_end_with_the_truncation_marker() {
        let mut field = question_field();
        field.description = Some("x".repeat(10_000));
        let message = question_message("proj", "", Some("question"), &[field]).unwrap();
        assert_eq!(message.message.len(), QUESTION_MAX_BYTES);
        assert!(message.message.ends_with(TRUNCATED));
    }

    #[test]
    fn first_characters_counts_code_points() {
        assert_eq!(first_characters("a\u{e9}b", 2), "a\u{e9}");
        assert_eq!(first_characters("abc", 10), "abc");
    }
}
