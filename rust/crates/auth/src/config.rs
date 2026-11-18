//! `ntfy-auth.json`: one principal, one wildcard row, one token file.
//!
//! Strict JSON, unknown fields fail, and the token file is a bare name inside
//! the install root. There is no second config source and no merge with the
//! relay or deploy config.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    pub fn new(message: impl Into<String>) -> Self {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Rw,
    Read,
    Write,
}

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rw => "rw",
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    /// The phrase `ntfy access` prints for this permission.
    pub fn access_text(self) -> &'static str {
        match self {
            Self::Rw => "read-write",
            Self::Read => "read-only",
            Self::Write => "write-only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub version: u32,
    pub container: String,
    pub user: String,
    pub topic: String,
    pub permission: Permission,
    #[serde(rename = "tokenLabel")]
    pub token_label: String,
    #[serde(rename = "tokenFile")]
    pub token_file: String,
}

impl AuthConfig {
    pub fn parse_text(text: &str) -> Result<Self, ConfigError> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let config: Self = serde_json::from_str(text)
            .map_err(|error| ConfigError::new(format!("config: invalid JSON: {error}")))?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|error| {
            ConfigError::new(format!("config: cannot read {}: {error}", path.display()))
        })?;
        Self::parse_text(&text)
    }

    /// The token file is a bare name beside the config, so one root holds every
    /// auth file and no path can point elsewhere.
    pub fn token_path(&self, root: &Path) -> PathBuf {
        root.join(&self.token_file)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != 1 {
            return Err(ConfigError::new(format!(
                "config: unsupported version {}, expected 1",
                self.version
            )));
        }
        if !valid_container(&self.container) {
            return Err(ConfigError::new(
                "config: container must be 1-64 characters of letters, digits, dot, dash, or underscore, and start with a letter or digit",
            ));
        }
        if !valid_name(&self.user) {
            return Err(ConfigError::new(
                "config: user must be 1-64 characters of letters, digits, dash, or underscore",
            ));
        }
        if !valid_topic(&self.topic) {
            return Err(ConfigError::new(
                "config: topic must be 1-64 characters of letters, digits, dash, underscore, or the wildcard *",
            ));
        }
        if !valid_label(&self.token_label) {
            return Err(ConfigError::new(
                "config: tokenLabel must be 1-64 characters of letters, digits, dot, dash, or underscore",
            ));
        }
        if !valid_file_name(&self.token_file) {
            return Err(ConfigError::new(
                "config: tokenFile must be a bare file name in the install root",
            ));
        }
        Ok(())
    }
}

fn valid_container(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_alphanumeric())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-' | b'.'))
}

fn valid_name(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-'))
}

fn valid_topic(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-' | b'*'))
}

fn valid_label(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-' | b'.'))
}

fn valid_file_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text != "."
        && text != ".."
        && !text.contains('/')
        && !text.contains('\\')
        && !text.contains(':')
        && text.chars().all(|ch| ch.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "container": "ntfy",
            "user": "phone",
            "topic": "*",
            "permission": "rw",
            "tokenLabel": "owner",
            "tokenFile": "ntfy-token.txt"
        })
    }

    fn parse(value: serde_json::Value) -> Result<AuthConfig, ConfigError> {
        AuthConfig::parse_text(&value.to_string())
    }

    #[test]
    fn minimal_config_parses() {
        let config = parse(minimal()).unwrap();
        assert_eq!(config.container, "ntfy");
        assert_eq!(config.user, "phone");
        assert_eq!(config.topic, "*");
        assert_eq!(config.permission, Permission::Rw);
        assert_eq!(config.token_label, "owner");
        assert_eq!(config.token_file, "ntfy-token.txt");
    }

    #[test]
    fn permissions_are_strict() {
        for (text, expected) in [
            ("rw", Permission::Rw),
            ("read", Permission::Read),
            ("write", Permission::Write),
        ] {
            let mut raw = minimal();
            raw["permission"] = serde_json::json!(text);
            assert_eq!(parse(raw).unwrap().permission, expected);
        }
        let mut raw = minimal();
        raw["permission"] = serde_json::json!("deny");
        assert!(parse(raw).is_err());
    }

    #[test]
    fn unknown_fields_fail() {
        let mut raw = minimal();
        raw["password"] = serde_json::json!("secret");
        let error = parse(raw).unwrap_err();
        assert!(error.message().contains("invalid JSON"), "{error}");
    }

    #[test]
    fn version_must_be_one() {
        let mut raw = minimal();
        raw["version"] = serde_json::json!(2);
        let error = parse(raw).unwrap_err();
        assert!(error.message().contains("unsupported version"), "{error}");
    }

    #[test]
    fn container_name_is_checked() {
        let long = "a".repeat(65);
        for bad in ["", "-ntfy", "nt fy", "ntfy/1", long.as_str()] {
            let mut raw = minimal();
            raw["container"] = serde_json::json!(bad);
            assert!(parse(raw).is_err(), "{bad}");
        }
        let mut raw = minimal();
        raw["container"] = serde_json::json!("ntfy.prod_1");
        assert!(parse(raw).is_ok());
    }

    #[test]
    fn user_and_topic_are_checked() {
        let long = "a".repeat(65);
        for bad in ["", "ph one", "ph*ne", long.as_str()] {
            let mut raw = minimal();
            raw["user"] = serde_json::json!(bad);
            assert!(parse(raw).is_err(), "{bad}");
        }
        for bad in ["", "proj one", long.as_str()] {
            let mut raw = minimal();
            raw["topic"] = serde_json::json!(bad);
            assert!(parse(raw).is_err(), "{bad}");
        }
        let mut raw = minimal();
        raw["topic"] = serde_json::json!("proj-*");
        assert!(parse(raw).is_ok());
    }

    #[test]
    fn token_file_is_a_bare_name() {
        for bad in [
            "",
            ".",
            "..",
            "sub/token.txt",
            "sub\\token.txt",
            "C:token.txt",
        ] {
            let mut raw = minimal();
            raw["tokenFile"] = serde_json::json!(bad);
            assert!(parse(raw).is_err(), "{bad}");
        }
    }

    #[test]
    fn token_path_stays_in_the_root() {
        let config = parse(minimal()).unwrap();
        assert_eq!(
            config.token_path(Path::new("/srv/relay")),
            Path::new("/srv/relay/ntfy-token.txt")
        );
    }

    #[test]
    fn bom_is_ignored() {
        let text = format!("\u{feff}{}", minimal());
        assert!(AuthConfig::parse_text(&text).is_ok());
    }
}
