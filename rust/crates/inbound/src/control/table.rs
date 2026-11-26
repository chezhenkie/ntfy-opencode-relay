//! Control table: one read-only JSON document, validated as a whole.
//!
//! Mirrors `relay-control.mjs`. A missing file selects the built-in table; a
//! file that exists is never merged and never written. Validation messages are
//! part of the contract and match the JavaScript strings.

use std::collections::HashMap;
use std::fmt;

use ntfy_relay_core::adapter::Capability;
use serde_json::{Map, Value};

pub const CONTROL_VERSION: u64 = 1;
pub const MAX_CONTROL_FILE_BYTES: u64 = 64 * 1024;
pub const CONFIRM_WINDOW_MS: i64 = 60_000;
pub const MAX_COOLDOWN_MS: u64 = 3_600_000;
pub const MAX_ACTIONS: usize = 64;
pub const MAX_PREFIXES: usize = 8;
pub const MAX_ALIASES: usize = 8;
pub const CONFIRM_EXPIRED_GRACE_MS: i64 = 300_000;
const MAX_ACK_CHARS: usize = 500;
const MAX_PREFIX_CHARS: usize = 32;
const MAX_VERB_CHARS: usize = 32;

/// A validation failure in the control document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlConfigError {
    message: String,
}

impl ControlConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ControlConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ControlConfigError {}

/// A failure while running an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlError {
    message: String,
}

impl ControlError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ControlError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Handler {
    SessionCreate,
    SessionList,
    SessionPin,
    SessionAuto,
    RelayHelp,
    RelayStatus,
}

impl Handler {
    pub const NAMES: [&'static str; 6] = [
        "session.create",
        "session.list",
        "session.pin",
        "session.auto",
        "relay.help",
        "relay.status",
    ];

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "session.create" => Some(Self::SessionCreate),
            "session.list" => Some(Self::SessionList),
            "session.pin" => Some(Self::SessionPin),
            "session.auto" => Some(Self::SessionAuto),
            "relay.help" => Some(Self::RelayHelp),
            "relay.status" => Some(Self::RelayStatus),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionCreate => "session.create",
            Self::SessionList => "session.list",
            Self::SessionPin => "session.pin",
            Self::SessionAuto => "session.auto",
            Self::RelayHelp => "relay.help",
            Self::RelayStatus => "relay.status",
        }
    }
}

impl fmt::Display for Handler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub handler: Handler,
    pub arg: Option<String>,
    pub ack: Option<String>,
    pub requires: Vec<Capability>,
    pub confirm: bool,
    /// Target the created session for its next message.
    pub target: bool,
    pub cooldown_ms: u64,
    pub enabled: bool,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ControlTable {
    pub version: u64,
    pub prefixes: Vec<String>,
    pub case_sensitive: bool,
    actions: Vec<(String, Action)>,
    lookup: HashMap<String, usize>,
}

impl ControlTable {
    /// Actions in document order, which is also help order.
    pub fn actions(&self) -> impl Iterator<Item = (&str, &Action)> {
        self.actions
            .iter()
            .map(|(verb, action)| (verb.as_str(), action))
    }

    pub fn action(&self, verb: &str) -> Option<&Action> {
        let key = fold(verb, self.case_sensitive);
        self.lookup.get(&key).map(|&index| &self.actions[index].1)
    }

    /// Canonical verb for a verb or alias key, already case-folded by the caller.
    pub fn verb_for_key(&self, key: &str) -> Option<&str> {
        self.lookup
            .get(key)
            .map(|&index| self.actions[index].0.as_str())
    }

    pub fn len(&self) -> usize {
        self.actions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

const RESERVED_VERBS: [&str; 7] = ["yes", "no", "steer", "allow", "always", "deny", "answer"];
const ACTION_FIELDS: [&str; 9] = [
    "handler",
    "aliases",
    "arg",
    "requires",
    "confirm",
    "ack",
    "target",
    "cooldownMs",
    "enabled",
];

pub fn builtin_control_table() -> ControlTable {
    let raw = serde_json::json!({
        "version": CONTROL_VERSION,
        "prefixes": ["/", "hey relay"],
        "caseSensitive": false,
        "actions": {
            "new": {
                "handler": "session.create",
                "requires": ["session.create"],
                "aliases": ["start"],
                "arg": "title",
                "ack": "new session {id} in {project}",
                "target": true,
                "cooldownMs": 5000
            },
            "sessions": { "handler": "session.list", "requires": ["session.list"], "arg": "count" },
            "to": {
                "handler": "session.pin",
                "requires": ["session.pin"],
                "arg": "session",
                "ack": "next message -> {title}"
            },
            "auto": { "handler": "session.auto", "requires": ["session.auto"], "ack": "target cleared" },
            "help": { "handler": "relay.help" },
            "status": { "handler": "relay.status" }
        }
    });
    validate_control_table(&raw).expect("built-in control table is valid")
}

pub fn validate_control_table(raw: &Value) -> Result<ControlTable, ControlConfigError> {
    let document = as_object(raw, "document must be an object")?;

    let version = document
        .get("version")
        .and_then(as_integer)
        .filter(|&value| value == CONTROL_VERSION as i64)
        .ok_or_else(|| ControlConfigError::new(format!("version must be {CONTROL_VERSION}")))?;

    let case_sensitive = document
        .get("caseSensitive")
        .and_then(Value::as_bool)
        .ok_or_else(|| ControlConfigError::new("caseSensitive must be a boolean"))?;

    let raw_prefixes = document
        .get("prefixes")
        .and_then(Value::as_array)
        .filter(|prefixes| !prefixes.is_empty() && prefixes.len() <= MAX_PREFIXES)
        .ok_or_else(|| {
            ControlConfigError::new(format!(
                "prefixes must be an array of 1 to {MAX_PREFIXES} strings"
            ))
        })?;

    let mut prefixes = Vec::with_capacity(raw_prefixes.len());
    let mut prefix_keys = Vec::with_capacity(raw_prefixes.len());
    for prefix in raw_prefixes {
        let text = prefix.as_str().ok_or_else(|| {
            ControlConfigError::new("prefixes: every prefix must be a non-empty string")
        })?;
        let value = trim_js(text);
        if value.is_empty() {
            return Err(ControlConfigError::new(
                "prefixes: every prefix must be a non-empty string",
            ));
        }
        if utf16_len(value) > MAX_PREFIX_CHARS {
            return Err(ControlConfigError::new(
                "prefixes: every prefix must be at most 32 characters",
            ));
        }
        let key = fold(value, case_sensitive);
        if prefix_keys.contains(&key) {
            return Err(ControlConfigError::new(format!(
                "prefixes: duplicate prefix {value}"
            )));
        }
        prefix_keys.push(key);
        prefixes.push(value.to_string());
    }

    let raw_actions = document
        .get("actions")
        .and_then(Value::as_object)
        .ok_or_else(|| ControlConfigError::new("actions must be an object"))?;
    if raw_actions.len() > MAX_ACTIONS {
        return Err(ControlConfigError::new(format!(
            "actions must hold at most {MAX_ACTIONS} entries"
        )));
    }

    let mut actions: Vec<(String, Action)> = Vec::with_capacity(raw_actions.len());
    let mut lookup: HashMap<String, usize> = HashMap::with_capacity(raw_actions.len());
    for (verb, raw_action) in raw_actions {
        if !is_valid_verb(verb) {
            return Err(ControlConfigError::new(format!(
                "actions.{verb}: verb must match /^[A-Za-z0-9][A-Za-z0-9_-]{{0,31}}$/"
            )));
        }
        let key = fold(verb, case_sensitive);
        if is_reserved(&key) {
            return Err(ControlConfigError::new(format!(
                "actions.{verb}: verb is reserved"
            )));
        }
        if lookup.contains_key(&key) {
            return Err(ControlConfigError::new(format!(
                "actions.{verb}: duplicate verb"
            )));
        }
        lookup.insert(key, actions.len());
        actions.push((verb.clone(), validate_action(verb, raw_action)?));
    }

    for (index, (verb, action)) in actions.iter().enumerate() {
        for alias in &action.aliases {
            let key = fold(alias, case_sensitive);
            if is_reserved(&key) {
                return Err(ControlConfigError::new(format!(
                    "actions.{verb}.aliases: {alias} is reserved"
                )));
            }
            if lookup.contains_key(&key) {
                return Err(ControlConfigError::new(format!(
                    "actions.{verb}.aliases: {alias} collides with another verb or alias"
                )));
            }
            lookup.insert(key, index);
        }
    }

    Ok(ControlTable {
        version: version as u64,
        prefixes,
        case_sensitive,
        actions,
        lookup,
    })
}

fn validate_action(verb: &str, raw: &Value) -> Result<Action, ControlConfigError> {
    let where_ = format!("actions.{verb}");
    let fields = as_object(raw, &format!("{where_}: must be an object"))?;
    for key in fields.keys() {
        if !ACTION_FIELDS.contains(&key.as_str()) {
            return Err(ControlConfigError::new(format!(
                "{where_}.{key}: unknown field"
            )));
        }
    }

    let handler = fields
        .get("handler")
        .and_then(Value::as_str)
        .and_then(Handler::parse)
        .ok_or_else(|| {
            ControlConfigError::new(format!(
                "{where_}.handler: must be one of {}",
                Handler::NAMES.join(", ")
            ))
        })?;

    let arg = match fields.get("arg") {
        None => None,
        Some(value) => {
            let text = value.as_str().ok_or_else(|| {
                ControlConfigError::new(format!("{where_}.arg: must be a non-empty string"))
            })?;
            if trim_js(text).is_empty() {
                return Err(ControlConfigError::new(format!(
                    "{where_}.arg: must be a non-empty string"
                )));
            }
            Some(trim_js(text).to_string())
        }
    };

    let ack = match fields.get("ack") {
        None => None,
        Some(value) => {
            let text = value.as_str().ok_or_else(|| {
                ControlConfigError::new(format!("{where_}.ack: must be a non-empty string"))
            })?;
            if trim_js(text).is_empty() {
                return Err(ControlConfigError::new(format!(
                    "{where_}.ack: must be a non-empty string"
                )));
            }
            if utf16_len(text) > MAX_ACK_CHARS {
                return Err(ControlConfigError::new(format!(
                    "{where_}.ack: must be at most {MAX_ACK_CHARS} characters"
                )));
            }
            Some(text.to_string())
        }
    };

    let requires = match fields.get("requires") {
        None => Vec::new(),
        Some(value) => {
            let items = value
                .as_array()
                .filter(|items| items.len() <= Capability::ALL.len());
            let items = items.ok_or_else(|| {
                ControlConfigError::new(format!(
                    "{where_}.requires: must be an array of capabilities"
                ))
            })?;
            let mut requires = Vec::with_capacity(items.len());
            for item in items {
                let name = item.as_str().ok_or_else(|| {
                    ControlConfigError::new(format!(
                        "{where_}.requires: unknown capability {}",
                        compact(item)
                    ))
                })?;
                let capability = Capability::parse(name).ok_or_else(|| {
                    ControlConfigError::new(format!("{where_}.requires: unknown capability {name}"))
                })?;
                if requires.contains(&capability) {
                    return Err(ControlConfigError::new(format!(
                        "{where_}.requires: duplicate capability {name}"
                    )));
                }
                requires.push(capability);
            }
            requires
        }
    };

    for flag in ["confirm", "target", "enabled"] {
        if let Some(value) = fields.get(flag) {
            if !value.is_boolean() {
                return Err(ControlConfigError::new(format!(
                    "{where_}.{flag}: must be a boolean"
                )));
            }
        }
    }

    let cooldown_ms = match fields.get("cooldownMs") {
        None => 0,
        Some(value) => {
            let number = as_integer(value)
                .filter(|&number| number >= 0 && number <= MAX_COOLDOWN_MS as i64)
                .ok_or_else(|| {
                    ControlConfigError::new(format!(
                        "{where_}.cooldownMs: must be an integer from 0 to {MAX_COOLDOWN_MS}"
                    ))
                })?;
            number as u64
        }
    };

    let aliases = match fields.get("aliases") {
        None => Vec::new(),
        Some(value) => {
            let items = value
                .as_array()
                .filter(|items| items.len() <= MAX_ALIASES)
                .ok_or_else(|| {
                    ControlConfigError::new(format!(
                        "{where_}.aliases: must be an array of at most {MAX_ALIASES} verbs"
                    ))
                })?;
            let mut aliases: Vec<String> = Vec::with_capacity(items.len());
            for item in items {
                let alias = item.as_str().ok_or_else(|| {
                    ControlConfigError::new(format!(
                        "{where_}.aliases: invalid alias {}",
                        compact(item)
                    ))
                })?;
                if !is_valid_verb(alias) {
                    return Err(ControlConfigError::new(format!(
                        "{where_}.aliases: invalid alias {alias}"
                    )));
                }
                if aliases.iter().any(|existing| existing == alias) {
                    return Err(ControlConfigError::new(format!(
                        "{where_}.aliases: duplicate alias"
                    )));
                }
                aliases.push(alias.to_string());
            }
            aliases
        }
    };

    Ok(Action {
        handler,
        arg,
        ack,
        requires,
        confirm: fields
            .get("confirm")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        target: fields
            .get("target")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        cooldown_ms,
        enabled: fields
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        aliases,
    })
}

fn as_object<'a>(
    value: &'a Value,
    message: &str,
) -> Result<&'a Map<String, Value>, ControlConfigError> {
    value
        .as_object()
        .ok_or_else(|| ControlConfigError::new(message))
}

fn as_integer(value: &Value) -> Option<i64> {
    let number = value.as_number()?;
    if let Some(integer) = number.as_i64() {
        return Some(integer);
    }
    let float = number.as_f64()?;
    if float.is_finite() && float.fract() == 0.0 {
        Some(float as i64)
    } else {
        None
    }
}

fn compact(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn fold(value: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        value.to_string()
    } else {
        value.to_lowercase()
    }
}

fn is_reserved(key: &str) -> bool {
    RESERVED_VERBS.contains(&key)
}

fn is_valid_verb(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    let mut length = 1;
    for character in chars {
        if !(character.is_ascii_alphanumeric() || character == '_' || character == '-') {
            return false;
        }
        length += 1;
    }
    length <= MAX_VERB_CHARS
}

pub fn trim_js(value: &str) -> &str {
    ntfy_relay_core::text::trim_js(value)
}

pub fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}
