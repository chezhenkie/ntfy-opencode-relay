//! Relay configuration: defaults, validation, home expansion, path resolution.
//!
//! Mirrors `loadConfigFromText` and `validateConfig` in
//! `ntfy-inbox-relay.mjs`, with one deliberate difference: `opencode.command`
//! is ignored. The Rust client talks HTTP directly, so no child process and no
//! PATH lookup is needed; existing Node config files still load unchanged.

use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::adapter::Delivery;

/// Defaults are paths inside the install root: a relative value resolves
/// against the config file directory, which is the root itself.
pub const DEFAULT_NTFY_TOKEN_FILE: &str = "ntfy-token.txt";
pub const DEFAULT_STATE_FILE: &str = "ntfy-relay-state.json";
pub const DEFAULT_LOG_FILE: &str = "ntfy-relay.log";
pub const DEFAULT_INBOX_DIR: &str = ".ntfy-inbox";
pub const DEFAULT_MAX_FILE_BYTES: u64 = 15 * 1024 * 1024;
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 16_384;
pub const DEFAULT_REPLAY_WINDOW_MINUTES: i64 = 60;
pub const DEFAULT_OPENCODE_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone, PartialEq)]
pub struct NtfyConfig {
    pub server: String,
    pub token_file: PathBuf,
    pub replay_window_minutes: f64,
    pub max_message_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpencodeConfig {
    /// Interim server address; discovery is still open (docs/specsheet 04, row 5).
    pub base_url: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelConfig {
    pub provider_id: String,
    pub id: String,
    pub variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterConfig {
    pub name: String,
    pub agent: Option<String>,
    pub model: Option<ModelConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingConfig {
    pub prefer_running: bool,
    pub delivery: Delivery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesConfig {
    pub inbox_dir: String,
    pub max_file_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundEvents {
    pub session_idle: bool,
    pub session_error: bool,
    pub permission_asked: bool,
    pub question_asked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppressSubagents {
    pub session_idle: bool,
    pub session_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnTextConfig {
    pub inline_max_bytes: usize,
    pub attachment_prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoAttachConfig {
    pub enabled: bool,
    pub extensions: Vec<String>,
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

/// Outbound push policy, mirroring the plugin defaults. The daemon starts with
/// `enabled: false` so the patched plugin stays the only publisher until the
/// cutover; the plugin is removed after `outbound` replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundConfig {
    pub enabled: bool,
    pub events: OutboundEvents,
    pub suppress_subagents: SuppressSubagents,
    pub permission_delay_ms: u64,
    pub turn_text: TurnTextConfig,
    pub auto_attach: AutoAttachConfig,
}

pub const DEFAULT_AUTO_ATTACH_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "svg", "avif", "heic", "mp3", "wav", "ogg",
    "flac", "m4a", "aac", "opus", "mp4", "webm", "mov", "mkv", "pdf", "txt", "md", "csv", "docx",
    "xlsx", "pptx", "epub", "zip", "7z", "tar", "gz",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    pub fn as_rank(self) -> u8 {
        match self {
            Self::Debug => 10,
            Self::Info => 20,
            Self::Warn => 30,
            Self::Error => 40,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoggingConfig {
    pub file: PathBuf,
    pub heartbeat_minutes: f64,
    pub level: LogLevel,
    pub echo: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub enabled: bool,
    pub ntfy: NtfyConfig,
    pub project_roots: Vec<PathBuf>,
    pub opencode: OpencodeConfig,
    pub adapter: AdapterConfig,
    pub routing: RoutingConfig,
    pub files: FilesConfig,
    pub outbound: OutboundConfig,
    pub logging: LoggingConfig,
    pub state_file: PathBuf,
    pub config_path: Option<PathBuf>,
    /// Top-level keys the loader does not read. The caller warns once.
    pub unknown_keys: Vec<String>,
}

/// Top-level config keys this loader reads.
const TOP_LEVEL_KEYS: &[&str] = &[
    "enabled",
    "ntfy",
    "projectRoots",
    "opencode",
    "adapter",
    "routing",
    "files",
    "outbound",
    "logging",
    "stateFile",
];

/// `~` and `~/` expansion. Absolute and other values are returned unchanged.
pub fn expand_home(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return home.join(rest);
    }
    PathBuf::from(value)
}

/// `USERPROFILE` first on Windows, then `HOME`, matching `os.homedir()`.
/// Unix reads `HOME` only; a stray `USERPROFILE` must not move the state,
/// token, log, or control paths.
pub fn home_dir() -> Option<PathBuf> {
    home_dir_from(|name| env::var_os(name))
}

fn home_dir_from(mut get: impl FnMut(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(value) = get("USERPROFILE").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(value));
    }
    get("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The install root: `NTFY_RELAY_HOME` for tests, else the directory that
/// holds the running binary. There is no home-directory fallback.
pub fn install_root() -> Option<PathBuf> {
    if let Some(value) = env::var_os("NTFY_RELAY_HOME") {
        if !value.is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

pub fn default_config_path(root: &Path) -> PathBuf {
    root.join("ntfy-relay.json")
}

/// Reads and validates one config file. Relative paths resolve against its
/// directory, which is what an operator editing the file expects.
pub fn load_config(file: &Path) -> Result<Config, ConfigError> {
    let text = fs::read_to_string(file).map_err(|error| {
        ConfigError::new(format!("config: cannot read {}: {error}", file.display()))
    })?;
    let home = home_dir();
    load_config_with_home(&text, Some(file), home.as_deref())
}

/// Testable core of `loadConfig`: JSON, defaults, validation, path resolution.
pub fn load_config_from_text(
    text: &str,
    config_path: Option<&Path>,
    home: &Path,
) -> Result<Config, ConfigError> {
    load_config_with_home(text, config_path, Some(home))
}

/// As [`load_config_from_text`], but the home directory is optional: it is
/// needed only to expand a `~` path, so a config of absolute and relative
/// paths loads on a host without `HOME` or `USERPROFILE`.
pub fn load_config_with_home(
    text: &str,
    config_path: Option<&Path>,
    home: Option<&Path>,
) -> Result<Config, ConfigError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let raw: Value = serde_json::from_str(text)
        .map_err(|error| ConfigError::new(format!("config: invalid JSON: {error}")))?;
    let mut merged = defaults();
    deep_merge(&mut merged, raw);
    let base_dir = match config_path {
        Some(path) => path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        None => env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    resolve(&merged, &base_dir, home, config_path.map(Path::to_path_buf))
}

fn defaults() -> Value {
    json!({
        "enabled": true,
        "ntfy": {
            "server": "",
            "tokenFile": DEFAULT_NTFY_TOKEN_FILE,
            "replayWindowMinutes": DEFAULT_REPLAY_WINDOW_MINUTES,
            "maxMessageBytes": DEFAULT_MAX_MESSAGE_BYTES,
        },
        "projectRoots": [],
        "opencode": {
            "baseUrl": null,
            "timeoutMs": DEFAULT_OPENCODE_TIMEOUT_MS,
        },
        "adapter": {
            "name": "opencode",
            "agent": null,
            "model": null,
        },
        "routing": {
            "preferRunning": true,
            "delivery": "queue",
        },
        "files": {
            "inboxDir": DEFAULT_INBOX_DIR,
            "maxFileBytes": DEFAULT_MAX_FILE_BYTES,
        },
        "outbound": {
            "enabled": false,
            "events": {
                "session.idle": true,
                "session.error": true,
                "permission.asked": true,
                "question.asked": true,
            },
            "suppressSubagents": {
                "session.idle": true,
                "session.error": true,
            },
            "permissionNotificationDelayMs": 15000,
            "turnText": {
                "inlineMaxBytes": 3400,
                "attachmentPrefix": "turn",
            },
            "autoAttach": {
                "enabled": true,
                "extensions": DEFAULT_AUTO_ATTACH_EXTENSIONS,
                "maxFiles": 8,
                "maxFileBytes": 10485760,
                "maxTotalBytes": 31457280,
            },
        },
        "logging": {
            "file": DEFAULT_LOG_FILE,
            "heartbeatMinutes": 5,
            "level": "info",
            "echo": false,
        },
        "stateFile": DEFAULT_STATE_FILE,
    })
}

/// Node `deepMerge`: objects merge recursively, everything else replaces.
fn deep_merge(base: &mut Value, extra: Value) {
    match (base, extra) {
        (Value::Object(base_map), Value::Object(extra_map)) => {
            for (key, value) in extra_map {
                match base_map.get_mut(&key) {
                    Some(slot) => deep_merge(slot, value),
                    None => {
                        base_map.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

fn field<'a>(parent: Option<&'a Value>, name: &str) -> Option<&'a Value> {
    parent.and_then(|value| value.get(name))
}

fn require_string(value: Option<&Value>, name: &str) -> Result<String, ConfigError> {
    match value.and_then(Value::as_str).map(str::trim) {
        Some(text) if !text.is_empty() => Ok(text.to_string()),
        _ => Err(ConfigError::new(format!(
            "config: {name} must be a non-empty string"
        ))),
    }
}

/// `http://` is allowed only for a loopback host; HTTPS is always allowed.
/// The wire layer in `ntfy` applies the same rule to publish and attach.
fn allowed_server(url: &url::Url) -> bool {
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

fn validate_server(value: &str) -> Result<(), ConfigError> {
    if !value.contains("://") {
        return Err(ConfigError::new(
            "config: ntfy.server must start with http:// or https://",
        ));
    }
    let url = url::Url::parse(value)
        .map_err(|_| ConfigError::new("config: ntfy.server is not a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError::new(
            "config: ntfy.server must start with http:// or https://",
        ));
    }
    if !allowed_server(&url) {
        return Err(ConfigError::new(
            "config: ntfy.server must use https unless the host is loopback",
        ));
    }
    Ok(())
}

/// A plain relative path: no root, no Windows drive or prefix, no `.` or `..`.
/// Rejecting the drive colon in text keeps `C:foo` a config error on Unix too.
fn valid_inbox_dir(value: &str) -> bool {
    if value.is_empty() || value.starts_with(['/', '\\']) || value.contains(':') {
        return false;
    }
    value
        .split(['/', '\\'])
        .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn resolve_path(value: &str, base_dir: &Path, home: Option<&Path>) -> Result<PathBuf, ConfigError> {
    let expanded = match home {
        Some(home) => expand_home(value, home),
        None if value == "~" || value.starts_with("~/") || value.starts_with("~\\") => {
            return Err(ConfigError::new(format!(
                "config: {value} needs a home directory to expand ~"
            )))
        }
        None => PathBuf::from(value),
    };
    Ok(if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    })
}

fn resolve(
    raw: &Value,
    base_dir: &Path,
    home: Option<&Path>,
    config_path: Option<PathBuf>,
) -> Result<Config, ConfigError> {
    let enabled = require_bool(raw.get("enabled"), "enabled")?;

    let ntfy = raw.get("ntfy");
    let server = require_string(field(ntfy, "server"), "ntfy.server")?;
    validate_server(&server)?;
    let token_file = require_string(field(ntfy, "tokenFile"), "ntfy.tokenFile")?;
    let replay_window_minutes = require_integer(
        field(ntfy, "replayWindowMinutes"),
        "ntfy.replayWindowMinutes",
        1,
        10_080,
    )? as f64;
    let max_message_bytes = require_integer(
        field(ntfy, "maxMessageBytes"),
        "ntfy.maxMessageBytes",
        1,
        1_048_576,
    )? as usize;

    // An empty list is a running relay with no topics; the control path can
    // remove the last root, so the loader must accept what it writes.
    let roots = match raw.get("projectRoots") {
        Some(Value::Array(items)) => items,
        _ => return Err(ConfigError::new("config: projectRoots must be an array")),
    };
    let mut project_roots = Vec::with_capacity(roots.len());
    for root in roots {
        let root = require_string(Some(root), "projectRoots[]")?;
        project_roots.push(resolve_path(&root, base_dir, home)?);
    }

    let opencode = raw.get("opencode");
    let base_url = match field(opencode, "baseUrl") {
        None | Some(Value::Null) => None,
        value => Some(require_string(value, "opencode.baseUrl")?),
    };
    if let Some(raw) = &base_url {
        crate::net::BaseUrl::parse(raw)
            .map_err(|error| ConfigError::new(format!("config: opencode.baseUrl: {error}")))?;
    }
    let timeout_ms = require_integer(
        field(opencode, "timeoutMs"),
        "opencode.timeoutMs",
        1,
        600_000,
    )? as u64;

    let adapter = raw.get("adapter");
    if !adapter.is_some_and(Value::is_object) {
        return Err(ConfigError::new("config: adapter must be an object"));
    }
    let name = require_string(field(adapter, "name"), "adapter.name")?;
    if name != "opencode" {
        return Err(ConfigError::new("config: adapter.name must be opencode"));
    }
    let agent = match field(adapter, "agent") {
        None | Some(Value::Null) => None,
        value => Some(require_string(value, "adapter.agent")?),
    };
    let model = match field(adapter, "model") {
        None | Some(Value::Null) => None,
        Some(value) if value.is_object() => {
            let provider_id =
                require_string(field(Some(value), "providerID"), "adapter.model.providerID")?;
            let id = require_string(field(Some(value), "id"), "adapter.model.id")?;
            let variant = match field(Some(value), "variant") {
                None | Some(Value::Null) => None,
                value => Some(require_string(value, "adapter.model.variant")?),
            };
            Some(ModelConfig {
                provider_id,
                id,
                variant,
            })
        }
        Some(_) => {
            return Err(ConfigError::new(
                "config: adapter.model must be an object or null",
            ))
        }
    };

    let routing = raw.get("routing");
    let delivery = match field(routing, "delivery").and_then(Value::as_str) {
        Some(value) => Delivery::parse(value)
            .ok_or_else(|| ConfigError::new("config: routing.delivery must be queue or steer"))?,
        None => {
            return Err(ConfigError::new(
                "config: routing.delivery must be queue or steer",
            ))
        }
    };
    let prefer_running = match field(routing, "preferRunning") {
        Some(Value::Bool(value)) => *value,
        _ => {
            return Err(ConfigError::new(
                "config: routing.preferRunning must be a boolean",
            ))
        }
    };

    let files = raw.get("files");
    let inbox_dir = require_string(field(files, "inboxDir"), "files.inboxDir")?;
    if !valid_inbox_dir(&inbox_dir) {
        return Err(ConfigError::new(
            "config: files.inboxDir must be a plain relative path without . or ..",
        ));
    }
    let max_file_bytes = require_integer(
        field(files, "maxFileBytes"),
        "files.maxFileBytes",
        1,
        16_777_216,
    )? as u64;

    let outbound = parse_outbound(raw.get("outbound"))?;

    let logging = raw.get("logging");
    let log_file = require_string(field(logging, "file"), "logging.file")?;
    let heartbeat_minutes = require_integer(
        field(logging, "heartbeatMinutes"),
        "logging.heartbeatMinutes",
        1,
        1_440,
    )? as f64;
    let level_text = field(logging, "level").and_then(Value::as_str);
    let level = level_text.and_then(LogLevel::parse).ok_or_else(|| {
        ConfigError::new("config: logging.level must be debug, info, warn, or error")
    })?;
    let echo = match field(logging, "echo") {
        Some(Value::Bool(value)) => *value,
        _ => return Err(ConfigError::new("config: logging.echo must be a boolean")),
    };

    let state_file = require_string(raw.get("stateFile"), "stateFile")?;

    let mut unknown_keys: Vec<String> = raw
        .as_object()
        .map(|object| {
            object
                .keys()
                .filter(|key| !TOP_LEVEL_KEYS.contains(&key.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    unknown_keys.sort();

    Ok(Config {
        enabled,
        ntfy: NtfyConfig {
            server: server.trim_end_matches('/').to_string(),
            token_file: resolve_path(&token_file, base_dir, home)?,
            replay_window_minutes,
            max_message_bytes,
        },
        project_roots,
        opencode: OpencodeConfig {
            base_url,
            timeout_ms,
        },
        adapter: AdapterConfig { name, agent, model },
        routing: RoutingConfig {
            prefer_running,
            delivery,
        },
        files: FilesConfig {
            inbox_dir,
            max_file_bytes,
        },
        outbound,
        logging: LoggingConfig {
            file: resolve_path(&log_file, base_dir, home)?,
            heartbeat_minutes,
            level,
            echo,
        },
        state_file: resolve_path(&state_file, base_dir, home)?,
        config_path,
        unknown_keys,
    })
}

fn require_bool(value: Option<&Value>, name: &str) -> Result<bool, ConfigError> {
    match value {
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(ConfigError::new(format!(
            "config: {name} must be a boolean"
        ))),
    }
}

fn require_integer(
    value: Option<&Value>,
    name: &str,
    min: i64,
    max: i64,
) -> Result<i64, ConfigError> {
    match value.and_then(Value::as_i64) {
        Some(value) if (min..=max).contains(&value) => Ok(value),
        _ => Err(ConfigError::new(format!(
            "config: {name} must be an integer between {min} and {max}"
        ))),
    }
}

fn require_object<'a>(
    value: Option<&'a Value>,
    name: &str,
) -> Result<&'a serde_json::Map<String, Value>, ConfigError> {
    value
        .and_then(Value::as_object)
        .ok_or_else(|| ConfigError::new(format!("config: {name} must be an object")))
}

fn reject_unknown_keys(
    object: &serde_json::Map<String, Value>,
    allowed: &[&str],
    name: &str,
) -> Result<(), ConfigError> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ConfigError::new(format!(
                "config: {name}.{key} is not a supported property"
            )));
        }
    }
    Ok(())
}

fn parse_outbound(raw: Option<&Value>) -> Result<OutboundConfig, ConfigError> {
    let raw = raw
        .filter(|value| value.is_object())
        .ok_or_else(|| ConfigError::new("config: outbound must be an object"))?;
    let enabled = require_bool(raw.get("enabled"), "outbound.enabled")?;

    let events = require_object(raw.get("events"), "outbound.events")?;
    reject_unknown_keys(
        events,
        &[
            "session.idle",
            "session.error",
            "permission.asked",
            "question.asked",
        ],
        "outbound.events",
    )?;
    let events = OutboundEvents {
        session_idle: require_bool(events.get("session.idle"), "outbound.events.session.idle")?,
        session_error: require_bool(events.get("session.error"), "outbound.events.session.error")?,
        permission_asked: require_bool(
            events.get("permission.asked"),
            "outbound.events.permission.asked",
        )?,
        question_asked: require_bool(
            events.get("question.asked"),
            "outbound.events.question.asked",
        )?,
    };

    let suppress = require_object(raw.get("suppressSubagents"), "outbound.suppressSubagents")?;
    reject_unknown_keys(
        suppress,
        &["session.idle", "session.error"],
        "outbound.suppressSubagents",
    )?;
    let suppress_subagents = SuppressSubagents {
        session_idle: require_bool(
            suppress.get("session.idle"),
            "outbound.suppressSubagents.session.idle",
        )?,
        session_error: require_bool(
            suppress.get("session.error"),
            "outbound.suppressSubagents.session.error",
        )?,
    };

    let permission_delay_ms = require_integer(
        raw.get("permissionNotificationDelayMs"),
        "outbound.permissionNotificationDelayMs",
        0,
        300_000,
    )? as u64;

    let turn_text = require_object(raw.get("turnText"), "outbound.turnText")?;
    reject_unknown_keys(
        turn_text,
        &["inlineMaxBytes", "attachmentPrefix"],
        "outbound.turnText",
    )?;
    let inline_max_bytes = require_integer(
        turn_text.get("inlineMaxBytes"),
        "outbound.turnText.inlineMaxBytes",
        1,
        4096,
    )? as usize;
    let attachment_prefix = turn_text
        .get("attachmentPrefix")
        .and_then(Value::as_str)
        .filter(|value| valid_attachment_prefix(value))
        .ok_or_else(|| {
            ConfigError::new(
                "config: outbound.turnText.attachmentPrefix must be 1-32 characters from A-Z a-z 0-9 . _ - and start alphanumeric",
            )
        })?
        .to_string();

    let auto_attach = require_object(raw.get("autoAttach"), "outbound.autoAttach")?;
    let auto_enabled = require_bool(auto_attach.get("enabled"), "outbound.autoAttach.enabled")?;
    let raw_extensions = auto_attach
        .get("extensions")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ConfigError::new("config: outbound.autoAttach.extensions must be an array")
        })?;
    let mut extensions = Vec::with_capacity(raw_extensions.len());
    for extension in raw_extensions {
        let value = extension.as_str().filter(|value| valid_extension(value));
        let Some(value) = value else {
            return Err(ConfigError::new(
                "config: outbound.autoAttach.extensions must contain 1-16 character alphanumeric extensions",
            ));
        };
        extensions.push(value.to_ascii_lowercase());
    }
    let max_files = require_integer(
        auto_attach.get("maxFiles"),
        "outbound.autoAttach.maxFiles",
        1,
        16,
    )? as usize;
    let max_file_bytes = require_integer(
        auto_attach.get("maxFileBytes"),
        "outbound.autoAttach.maxFileBytes",
        1,
        20_971_520,
    )? as u64;
    let max_total_bytes = require_integer(
        auto_attach.get("maxTotalBytes"),
        "outbound.autoAttach.maxTotalBytes",
        max_file_bytes as i64,
        41_943_040,
    )? as u64;

    Ok(OutboundConfig {
        enabled,
        events,
        suppress_subagents,
        permission_delay_ms,
        turn_text: TurnTextConfig {
            inline_max_bytes,
            attachment_prefix,
        },
        auto_attach: AutoAttachConfig {
            enabled: auto_enabled,
            extensions,
            max_files,
            max_file_bytes,
            max_total_bytes,
        },
    })
}

fn valid_attachment_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 || !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_extension(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty() && bytes.len() <= 16 && bytes.iter().all(u8::is_ascii_alphanumeric)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("core-config-tests")
    }

    fn home() -> PathBuf {
        root().join("home")
    }

    fn base() -> PathBuf {
        root().join("cfg-dir")
    }

    fn minimal() -> Value {
        json!({
            "ntfy": { "server": "https://ntfy.test" },
            "projectRoots": ["projects"],
        })
    }

    #[test]
    fn partial_config_merges_defaults_and_resolves_paths() {
        let config = load_config_from_text(
            &json!({
                "ntfy": {
                    "server": "https://ntfy.test/",
                    "tokenFile": "~/alt-config/token.txt"
                },
                "projectRoots": ["projects"],
                "logging": { "file": "~/alt-state/relay.log" },
                "stateFile": "~/alt-state/state.json"
            })
            .to_string(),
            Some(&base().join("ntfy-relay.json")),
            &home(),
        )
        .unwrap();

        assert_eq!(config.ntfy.server, "https://ntfy.test");
        assert_eq!(config.ntfy.token_file, home().join("alt-config/token.txt"));
        assert_eq!(config.project_roots[0], base().join("projects"));
        assert_eq!(config.logging.file, home().join("alt-state/relay.log"));
        assert_eq!(config.state_file, home().join("alt-state/state.json"));
        assert_eq!(config.config_path, Some(base().join("ntfy-relay.json")));
    }

    #[test]
    fn defaults_keep_the_node_caps() {
        let config = load_config_from_text(&minimal().to_string(), None, &home()).unwrap();
        assert!(config.enabled);
        assert_eq!(config.ntfy.replay_window_minutes, 60.0);
        assert_eq!(config.ntfy.max_message_bytes, 16_384);
        assert_eq!(config.files.inbox_dir, ".ntfy-inbox");
        assert_eq!(config.files.max_file_bytes, 15 * 1024 * 1024);
        assert_eq!(config.routing.delivery, Delivery::Queue);
        assert!(config.routing.prefer_running);
        assert_eq!(config.logging.level, LogLevel::Info);
        assert!(!config.logging.echo);
        assert_eq!(config.opencode.timeout_ms, 30_000);
        assert_eq!(config.adapter.name, "opencode");
    }

    #[test]
    fn defaults_live_in_the_install_root() {
        let config = load_config_from_text(
            &minimal().to_string(),
            Some(&base().join("ntfy-relay.json")),
            &home(),
        )
        .unwrap();
        assert_eq!(config.ntfy.token_file, base().join("ntfy-token.txt"));
        assert_eq!(config.logging.file, base().join("ntfy-relay.log"));
        assert_eq!(config.state_file, base().join("ntfy-relay-state.json"));
        assert_eq!(default_config_path(&base()), base().join("ntfy-relay.json"));
    }

    #[test]
    fn invalid_documents_are_rejected() {
        let error = load_config_from_text("{", None, &home()).unwrap_err();
        assert!(error.message().contains("invalid JSON"), "{error}");

        let mut empty_roots = minimal();
        empty_roots["projectRoots"] = json!("projects");
        let error = load_config_from_text(&empty_roots.to_string(), None, &home()).unwrap_err();
        assert!(error.message().contains("projectRoots"), "{error}");

        let mut escape = minimal();
        escape["files"] = json!({ "inboxDir": "../escape" });
        let error = load_config_from_text(&escape.to_string(), None, &home()).unwrap_err();
        assert!(error.message().contains("inboxDir"), "{error}");

        let mut delivery = minimal();
        delivery["routing"] = json!({ "delivery": "later" });
        let error = load_config_from_text(&delivery.to_string(), None, &home()).unwrap_err();
        assert!(error.message().contains("delivery"), "{error}");

        let mut scheme = minimal();
        scheme["ntfy"]["server"] = json!("ntfy.test");
        let error = load_config_from_text(&scheme.to_string(), None, &home()).unwrap_err();
        assert!(error.message().contains("http://"), "{error}");

        let mut wrong_adapter = minimal();
        wrong_adapter["adapter"] = json!({ "name": "pi" });
        let error = load_config_from_text(&wrong_adapter.to_string(), None, &home()).unwrap_err();
        assert!(error.message().contains("adapter.name"), "{error}");
    }

    #[test]
    fn unknown_top_level_keys_are_reported_once_and_do_not_block() {
        let mut raw = minimal();
        raw["ntfyServe"] = json!({ "server": "https://elsewhere" });
        raw["stateFil"] = json!("typo");
        let config = load_config_from_text(
            &raw.to_string(),
            Some(&base().join("ntfy-relay.json")),
            &home(),
        )
        .unwrap();
        assert_eq!(config.unknown_keys, vec!["ntfyServe", "stateFil"]);
        // The known value still wins over the unknown look-alike.
        assert_eq!(config.ntfy.server, "https://ntfy.test");
        assert_eq!(config.state_file, base().join(DEFAULT_STATE_FILE));

        let plain = load_config_from_text(&minimal().to_string(), None, &home()).unwrap();
        assert!(plain.unknown_keys.is_empty());
    }

    #[test]
    fn absolute_paths_load_without_a_home_directory() {
        let raw = json!({
            "ntfy": { "server": "https://ntfy.test", "tokenFile": "/abs/token.txt" },
            "projectRoots": ["/srv/projects"],
            "logging": { "file": "/var/log/relay.log" },
            "stateFile": "/var/lib/relay-state.json"
        });
        let config = load_config_with_home(
            &raw.to_string(),
            Some(&base().join("ntfy-relay.json")),
            None,
        )
        .unwrap();
        assert_eq!(config.ntfy.token_file, PathBuf::from("/abs/token.txt"));
        assert_eq!(config.project_roots, vec![PathBuf::from("/srv/projects")]);
        assert_eq!(config.logging.file, PathBuf::from("/var/log/relay.log"));
        assert_eq!(
            config.state_file,
            PathBuf::from("/var/lib/relay-state.json")
        );

        let mut tilde = minimal();
        tilde["stateFile"] = json!("~/state.json");
        let error = load_config_with_home(&tilde.to_string(), None, None).unwrap_err();
        assert!(error.message().contains("home directory"), "{error}");
    }

    #[test]
    fn home_expansion_leaves_absolute_paths_alone() {
        let home = home();
        assert_eq!(expand_home("~", &home), home);
        assert_eq!(expand_home("~/x/y", &home), home.join("x/y"));
        assert_eq!(expand_home("~\\x", &home), home.join("x"));
        assert_eq!(
            expand_home("/already/abs", &home),
            PathBuf::from("/already/abs")
        );
        assert_eq!(expand_home("relative", &home), PathBuf::from("relative"));
    }

    #[test]
    fn adapter_agent_and_model_are_optional() {
        let mut raw = minimal();
        raw["adapter"] = json!({ "name": "opencode", "agent": null, "model": null });
        let config = load_config_from_text(&raw.to_string(), None, &home()).unwrap();
        assert_eq!(config.adapter.agent, None);
        assert_eq!(config.adapter.model, None);

        raw["adapter"] = json!({
            "name": "opencode",
            "agent": "build",
            "model": { "providerID": "p", "id": "m", "variant": "max" }
        });
        let config = load_config_from_text(&raw.to_string(), None, &home()).unwrap();
        assert_eq!(config.adapter.agent.as_deref(), Some("build"));
        assert_eq!(
            config.adapter.model,
            Some(ModelConfig {
                provider_id: "p".to_string(),
                id: "m".to_string(),
                variant: Some("max".to_string()),
            })
        );
    }

    #[test]
    fn outbound_defaults_match_the_plugin_and_stay_disabled() {
        let config = load_config_from_text(&minimal().to_string(), None, &home()).unwrap();
        let outbound = &config.outbound;
        assert!(!outbound.enabled);
        assert!(outbound.events.session_idle);
        assert!(outbound.events.session_error);
        assert!(outbound.events.permission_asked);
        assert!(outbound.events.question_asked);
        assert!(outbound.suppress_subagents.session_idle);
        assert!(outbound.suppress_subagents.session_error);
        assert_eq!(outbound.permission_delay_ms, 15_000);
        assert_eq!(outbound.turn_text.inline_max_bytes, 3400);
        assert_eq!(outbound.turn_text.attachment_prefix, "turn");
        assert!(outbound.auto_attach.enabled);
        assert_eq!(outbound.auto_attach.max_files, 8);
        assert_eq!(outbound.auto_attach.max_file_bytes, 10_485_760);
        assert_eq!(outbound.auto_attach.max_total_bytes, 31_457_280);
        assert!(outbound.auto_attach.extensions.contains(&"png".to_string()));
        assert!(outbound.auto_attach.extensions.contains(&"gz".to_string()));
    }

    #[test]
    fn outbound_values_are_validated() {
        let mut raw = minimal();
        raw["outbound"] = json!({
            "enabled": true,
            "events": { "session.idle": false },
            "permissionNotificationDelayMs": 0,
            "turnText": { "inlineMaxBytes": 100, "attachmentPrefix": "turn-1" },
            "autoAttach": {
                "enabled": false,
                "extensions": ["MD", "png"],
                "maxFiles": 2,
                "maxFileBytes": 1000,
                "maxTotalBytes": 2000
            }
        });
        let config = load_config_from_text(&raw.to_string(), None, &home()).unwrap();
        assert!(config.outbound.enabled);
        assert!(!config.outbound.events.session_idle);
        assert!(config.outbound.events.session_error);
        assert_eq!(config.outbound.permission_delay_ms, 0);
        assert_eq!(config.outbound.turn_text.inline_max_bytes, 100);
        assert_eq!(config.outbound.turn_text.attachment_prefix, "turn-1");
        assert!(!config.outbound.auto_attach.enabled);
        assert_eq!(config.outbound.auto_attach.extensions, vec!["md", "png"]);
        assert_eq!(config.outbound.auto_attach.max_files, 2);

        let mut bad_delay = minimal();
        bad_delay["outbound"] = json!({ "permissionNotificationDelayMs": 300001 });
        let error = load_config_from_text(&bad_delay.to_string(), None, &home()).unwrap_err();
        assert!(
            error.message().contains("permissionNotificationDelayMs"),
            "{error}"
        );

        let mut bad_prefix = minimal();
        bad_prefix["outbound"] = json!({ "turnText": { "attachmentPrefix": "-turn" } });
        let error = load_config_from_text(&bad_prefix.to_string(), None, &home()).unwrap_err();
        assert!(error.message().contains("attachmentPrefix"), "{error}");

        let mut bad_total = minimal();
        bad_total["outbound"] =
            json!({ "autoAttach": { "maxFileBytes": 2000, "maxTotalBytes": 1000 } });
        let error = load_config_from_text(&bad_total.to_string(), None, &home()).unwrap_err();
        assert!(error.message().contains("maxTotalBytes"), "{error}");

        let mut bad_event = minimal();
        bad_event["outbound"] = json!({ "events": { "session.turn": true } });
        let error = load_config_from_text(&bad_event.to_string(), None, &home()).unwrap_err();
        assert!(error.message().contains("session.turn"), "{error}");
    }

    #[test]
    fn bom_and_open_code_command_are_accepted() {
        let mut raw = minimal();
        raw["opencode"] = json!({
            "command": ["opencode"],
            "baseUrl": "http://127.0.0.1:4096",
            "timeoutMs": 30000
        });
        let text = format!("\u{feff}{}", raw);
        let config = load_config_from_text(&text, None, &home()).unwrap();
        assert_eq!(
            config.opencode.base_url.as_deref(),
            Some("http://127.0.0.1:4096")
        );
    }

    #[test]
    fn opencode_base_url_follows_the_transport_rule() {
        let mut raw = minimal();
        raw["opencode"] = json!({ "baseUrl": "http://192.168.1.20:4096" });
        let error = load_config_from_text(&raw.to_string(), None, &home()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("config: opencode.baseUrl: base URL must use HTTPS"),
            "{error}"
        );

        raw["opencode"] = json!({ "baseUrl": "https://oc.example" });
        let config = load_config_from_text(&raw.to_string(), None, &home()).unwrap();
        assert_eq!(
            config.opencode.base_url.as_deref(),
            Some("https://oc.example")
        );

        raw["opencode"] = json!({ "baseUrl": "http://127.0.0.1:4096/api" });
        let error = load_config_from_text(&raw.to_string(), None, &home()).unwrap_err();
        assert!(
            error.to_string().contains("must not carry a path"),
            "{error}"
        );
    }

    #[test]
    fn plain_http_is_accepted_only_on_loopback() {
        for server in [
            "http://127.0.0.1:8810",
            "http://localhost:8810",
            "http://[::1]:8810",
            "https://ntfy.example",
        ] {
            let mut raw = minimal();
            raw["ntfy"]["server"] = json!(server);
            assert!(
                load_config_from_text(&raw.to_string(), None, &home()).is_ok(),
                "{server}"
            );
        }
        for server in ["http://ntfy.example", "http://203.0.113.10:8810"] {
            let mut raw = minimal();
            raw["ntfy"]["server"] = json!(server);
            let error = load_config_from_text(&raw.to_string(), None, &home()).unwrap_err();
            assert!(error.message().contains("https"), "{server}: {error}");
        }
    }

    #[test]
    fn booleans_are_strict_including_enabled() {
        for value in [json!("false"), json!(0), json!(null)] {
            let mut raw = minimal();
            raw["enabled"] = value;
            let error = load_config_from_text(&raw.to_string(), None, &home()).unwrap_err();
            assert!(error.message().contains("enabled"), "{error}");
        }
        let mut raw = minimal();
        raw["enabled"] = json!(false);
        let config = load_config_from_text(&raw.to_string(), None, &home()).unwrap();
        assert!(!config.enabled);
    }

    #[test]
    fn numeric_limits_are_integers_within_bounds() {
        for value in [json!(0.5), json!(0), json!(1e300), json!("60")] {
            let mut raw = minimal();
            raw["ntfy"]["maxMessageBytes"] = value;
            let error = load_config_from_text(&raw.to_string(), None, &home()).unwrap_err();
            assert!(error.message().contains("maxMessageBytes"), "{error}");
        }
        for value in [json!(0.5), json!(0), json!(1e300)] {
            let mut raw = minimal();
            raw["ntfy"]["replayWindowMinutes"] = value;
            let error = load_config_from_text(&raw.to_string(), None, &home()).unwrap_err();
            assert!(error.message().contains("replayWindowMinutes"), "{error}");
        }
        let mut raw = minimal();
        raw["logging"]["heartbeatMinutes"] = json!(1e300);
        let error = load_config_from_text(&raw.to_string(), None, &home()).unwrap_err();
        assert!(error.message().contains("heartbeatMinutes"), "{error}");
    }

    #[test]
    fn inbox_dir_must_be_a_plain_relative_path() {
        for value in ["C:foo", "/abs", "..", "../escape", "a/../../b", ".", ""] {
            let mut raw = minimal();
            raw["files"] = json!({ "inboxDir": value });
            let error = load_config_from_text(&raw.to_string(), None, &home()).unwrap_err();
            assert!(error.message().contains("inboxDir"), "{value}: {error}");
        }
        let mut raw = minimal();
        raw["files"] = json!({ "inboxDir": "inbox/files" });
        let config = load_config_from_text(&raw.to_string(), None, &home()).unwrap();
        assert_eq!(config.files.inbox_dir, "inbox/files");
    }

    #[test]
    fn several_project_roots_are_allowed() {
        let mut raw = minimal();
        raw["projectRoots"] = json!(["one", "two"]);
        let config = load_config_from_text(
            &raw.to_string(),
            Some(&base().join("ntfy-relay.json")),
            &home(),
        )
        .unwrap();
        assert_eq!(
            config.project_roots,
            vec![base().join("one"), base().join("two")]
        );
    }

    #[test]
    fn an_empty_root_list_serves_nothing() {
        let mut raw = minimal();
        raw["projectRoots"] = json!([]);
        let config = load_config_from_text(&raw.to_string(), None, &home()).unwrap();
        assert!(config.project_roots.is_empty());
    }

    #[test]
    fn home_dir_reads_home_on_unix_even_with_userprofile() {
        let result = home_dir_from(|name| match name {
            "USERPROFILE" => Some(std::ffi::OsString::from("/foreign")),
            "HOME" => Some(std::ffi::OsString::from("/home/real")),
            _ => None,
        });
        #[cfg(unix)]
        assert_eq!(result, Some(PathBuf::from("/home/real")));
        #[cfg(windows)]
        assert_eq!(result, Some(PathBuf::from("/foreign")));
    }
}
