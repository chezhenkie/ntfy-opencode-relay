//! Parsing of control prefixes, verbs, and native commands.
//!
//! Mirrors `relay-control.mjs`. Parsing is pure and returns a decision; the
//! capability gate, confirm holds, and cooldowns live in the engine.

use std::collections::HashMap;

use ntfy_relay_core::text::is_js_space;

use super::table::{trim_js, ControlTable};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// Free text, delivered as a prompt.
    Prompt,
    /// Unknown command after an explicit phrase prefix ("hey relay").
    Unknown { phrase: bool },
    /// A `/yes` or `/no` answer.
    Confirm { verb: String },
    /// A verb that resolves to the table.
    Action {
        verb: String,
        arg: String,
        phrase: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeCommand {
    pub verb: &'static str,
    pub arg: Option<&'static str>,
    pub help: &'static str,
}

pub const NATIVE_COMMANDS: [NativeCommand; 5] = [
    NativeCommand {
        verb: "steer",
        arg: Some("text"),
        help: "/steer <text> - interrupt the running turn",
    },
    NativeCommand {
        verb: "allow",
        arg: None,
        help: "/allow [code] - approve the newest ask, or the ask with that code",
    },
    NativeCommand {
        verb: "always",
        arg: None,
        help: "/always [code] - approve that ask for the session",
    },
    NativeCommand {
        verb: "deny",
        arg: None,
        help: "/deny [code] - reject the newest ask, or the ask with that code",
    },
    NativeCommand {
        verb: "answer",
        arg: Some("text"),
        help:
            "/answer - dismiss the last question and send the rest of the message to the assistant",
    },
];

const RESERVED_VERBS: [&str; 7] = ["yes", "no", "steer", "allow", "always", "deny", "answer"];

pub fn parse_control(text: &str, table: &ControlTable) -> Parsed {
    let body = trim_js(text);
    if body.is_empty() {
        return Parsed::Prompt;
    }

    for prefix in &table.prefixes {
        let Some(rest) = strip_prefix(body, prefix, table.case_sensitive) else {
            continue;
        };
        let phrase = prefix != "/";
        if phrase && !rest.is_empty() && !rest.chars().next().is_some_and(is_js_space) {
            continue;
        }
        let trimmed = trim_js(rest);
        if trimmed.is_empty() {
            return if phrase {
                Parsed::Unknown { phrase }
            } else {
                Parsed::Prompt
            };
        }

        let (verb, arg) = split_verb(trimmed);
        let key = if table.case_sensitive {
            verb.to_string()
        } else {
            verb.to_lowercase()
        };

        if RESERVED_VERBS.contains(&key.as_str()) {
            return if NATIVE_COMMANDS.iter().any(|command| command.verb == key) {
                Parsed::Prompt
            } else {
                Parsed::Confirm { verb: key }
            };
        }

        return match table.verb_for_key(&key) {
            Some(canonical) => Parsed::Action {
                verb: canonical.to_string(),
                arg,
                phrase,
            },
            None if phrase => Parsed::Unknown { phrase },
            None => Parsed::Prompt,
        };
    }

    Parsed::Prompt
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedNative {
    pub verb: &'static str,
    pub arg: String,
}

impl ParsedNative {
    /// The optional code token. `#` and case are normalized by the registry.
    pub fn arg_token(&self) -> Option<&str> {
        let token = self.arg.trim();
        if token.is_empty() {
            None
        } else {
            Some(token)
        }
    }
}

pub fn parse_native_command(text: &str) -> Option<ParsedNative> {
    let body = trim_js(text);
    let rest = body.strip_prefix('/')?;
    if rest.is_empty() {
        return None;
    }
    let (verb, arg) = split_verb(rest);
    let verb = verb.to_lowercase();
    NATIVE_COMMANDS
        .iter()
        .find(|command| command.verb == verb)
        .map(|command| ParsedNative {
            verb: command.verb,
            arg,
        })
}

pub fn render_ack(template: &str, values: &HashMap<String, String>) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        rendered.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let mut end = None;
        let mut valid = true;
        for (index, character) in after.char_indices() {
            if character == '}' {
                end = Some(index);
                break;
            }
            if !(character.is_ascii_alphanumeric() || character == '_') {
                valid = false;
                break;
            }
        }
        match (valid, end) {
            (true, Some(index)) if index > 0 => {
                let key = &after[..index];
                match values.get(key) {
                    Some(value) => rendered.push_str(value),
                    None => {
                        rendered.push('{');
                        rendered.push_str(key);
                        rendered.push('}');
                    }
                }
                rest = &after[index + 1..];
            }
            _ => {
                rendered.push('{');
                rest = after;
            }
        }
    }
    rendered.push_str(rest);
    rendered
}

fn split_verb(value: &str) -> (String, String) {
    let mut split = value.char_indices();
    let mut end = value.len();
    for (index, character) in split.by_ref() {
        if is_js_space(character) {
            end = index;
            break;
        }
    }
    let verb = value[..end].to_string();
    let arg = trim_js(&value[end..]).to_string();
    (verb, arg)
}

fn strip_prefix<'a>(text: &'a str, prefix: &str, case_sensitive: bool) -> Option<&'a str> {
    if case_sensitive {
        return text.strip_prefix(prefix);
    }
    let mut subject = text.chars();
    for expected in prefix.chars() {
        let actual = subject.next()?;
        if !chars_eq_ignore_case(actual, expected) {
            return None;
        }
    }
    Some(subject.as_str())
}

fn chars_eq_ignore_case(left: char, right: char) -> bool {
    left == right || left.to_lowercase().eq(right.to_lowercase())
}
