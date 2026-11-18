//! Deployment config: JSON shape, strict parsing, URL and mode rules.
//!
//! One file, `ntfy-deploy.json`, in the relay install root. Unknown fields and
//! unknown modes fail; there is no legacy shape and no merge with the relay
//! config.

use std::fmt;
use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;
use std::str::FromStr;

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
pub enum Mode {
    Proxy,
    Vpn,
    Lan,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Vpn => "vpn",
            Self::Lan => "lan",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// A validated `baseUrl`: scheme, host, and an optional explicit port. Paths,
/// queries, fragments, credentials, trailing slashes, and whitespace are
/// rejected before the value is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl {
    scheme: Scheme,
    host: String,
    port: Option<u16>,
}

impl BaseUrl {
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        if raw.is_empty() {
            return Err(ConfigError::new("baseUrl must not be empty"));
        }
        if raw.chars().any(char::is_whitespace) {
            return Err(ConfigError::new("baseUrl must not contain whitespace"));
        }
        if raw.contains('?') || raw.contains('#') {
            return Err(ConfigError::new(
                "baseUrl must not contain a query or fragment",
            ));
        }
        let (scheme, rest) = if let Some(rest) = raw.strip_prefix("https://") {
            (Scheme::Https, rest)
        } else if let Some(rest) = raw.strip_prefix("http://") {
            (Scheme::Http, rest)
        } else {
            return Err(ConfigError::new(
                "baseUrl must start with https:// or http://",
            ));
        };
        if rest.is_empty() {
            return Err(ConfigError::new("baseUrl must include a host"));
        }
        if rest.contains('@') {
            return Err(ConfigError::new("baseUrl must not contain credentials"));
        }
        if rest.contains('\\') {
            return Err(ConfigError::new("baseUrl must not contain a backslash"));
        }
        if rest.ends_with('/') {
            return Err(ConfigError::new("baseUrl must not end with a slash"));
        }
        if rest.contains('/') {
            return Err(ConfigError::new("baseUrl must not contain a path"));
        }
        let (host, port) = split_host_port(rest)?;
        let host = validate_host(host)?;
        Ok(Self { scheme, host, port })
    }

    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> Option<u16> {
        self.port
    }

    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or(match self.scheme {
            Scheme::Http => 80,
            Scheme::Https => 443,
        })
    }

    pub fn is_loopback(&self) -> bool {
        is_loopback_host(&self.host)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerConfig {
    pub name: String,
    pub image: String,
    #[serde(rename = "cacheVolume")]
    pub cache_volume: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VpnConfig {
    pub ip: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeployConfig {
    pub version: u32,
    pub mode: Mode,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    #[serde(rename = "behindProxy")]
    pub behind_proxy: bool,
    pub port: u16,
    pub container: ContainerConfig,
    #[serde(default)]
    pub vpn: Option<VpnConfig>,
}

impl DeployConfig {
    /// Parses and validates one config document.
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

    /// The canonical document, two-space indented and without a trailing
    /// newline. Field order follows the struct and matches the example file.
    pub fn to_json(&self) -> Result<String, ConfigError> {
        serde_json::to_string_pretty(self)
            .map_err(|error| ConfigError::new(format!("config: cannot serialize: {error}")))
    }

    pub fn base_url(&self) -> Result<BaseUrl, ConfigError> {
        BaseUrl::parse(&self.base_url)
    }

    pub fn vpn_ip(&self) -> Option<Ipv4Addr> {
        self.vpn
            .as_ref()
            .and_then(|vpn| Ipv4Addr::from_str(&vpn.ip).ok())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != 1 {
            return Err(ConfigError::new(format!(
                "config: unsupported version {}, expected 1",
                self.version
            )));
        }
        if self.port == 0 {
            return Err(ConfigError::new("config: port must be 1-65535"));
        }
        let url = self.base_url()?;
        if self.mode == Mode::Proxy && url.scheme() != Scheme::Https {
            return Err(ConfigError::new(
                "config: proxy mode requires an https baseUrl",
            ));
        }
        if url.scheme() == Scheme::Http {
            if self.mode == Mode::Proxy {
                return Err(ConfigError::new(
                    "config: http baseUrl is not allowed in proxy mode",
                ));
            }
            if !url.is_loopback() && !is_private_host(url.host()) {
                return Err(ConfigError::new(
                    "config: http baseUrl host must be a loopback, VPN, or LAN address",
                ));
            }
            if url.port_or_default() != self.port {
                return Err(ConfigError::new(format!(
                    "config: http baseUrl port {} must match port {}",
                    url.port_or_default(),
                    self.port
                )));
            }
        }
        if self.behind_proxy != (self.mode == Mode::Proxy) {
            return Err(ConfigError::new(
                "config: behindProxy is true only in proxy mode",
            ));
        }
        validate_name("container.name", &self.container.name)?;
        validate_name("container.cacheVolume", &self.container.cache_volume)?;
        if self.container.image.is_empty()
            || self.container.image.starts_with('-')
            || self.container.image.chars().any(char::is_whitespace)
        {
            return Err(ConfigError::new(
                "config: container.image must be a non-empty reference without whitespace",
            ));
        }
        if self.mode == Mode::Vpn {
            let vpn = self
                .vpn
                .as_ref()
                .ok_or_else(|| ConfigError::new("config: vpn.ip is required in vpn mode"))?;
            validate_vpn_ip(&vpn.ip)?;
        } else if let Some(vpn) = self.vpn.as_ref() {
            validate_vpn_ip(&vpn.ip)?;
        }
        Ok(())
    }
}

fn split_host_port(rest: &str) -> Result<(&str, Option<u16>), ConfigError> {
    if rest.starts_with('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| ConfigError::new("baseUrl has an unterminated IPv6 host"))?;
        let host = &rest[1..end];
        let tail = &rest[end + 1..];
        if tail.is_empty() {
            return Ok((host, None));
        }
        let port = tail
            .strip_prefix(':')
            .ok_or_else(|| ConfigError::new("baseUrl has an invalid host"))?;
        return Ok((host, Some(parse_port(port)?)));
    }
    let colons = rest.matches(':').count();
    if colons > 1 {
        return Err(ConfigError::new(
            "baseUrl must bracket an IPv6 host address",
        ));
    }
    match rest.rsplit_once(':') {
        Some((host, port)) => Ok((host, Some(parse_port(port)?))),
        None => Ok((rest, None)),
    }
}

fn parse_port(text: &str) -> Result<u16, ConfigError> {
    match text.parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err(ConfigError::new("baseUrl has an invalid port")),
    }
}

fn validate_host(host: &str) -> Result<String, ConfigError> {
    if host.is_empty() {
        return Err(ConfigError::new("baseUrl must include a host"));
    }
    if host.parse::<Ipv4Addr>().is_ok() {
        return Ok(host.to_string());
    }
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        return Ok(host.to_string());
    }
    if host.len() > 253 || host.ends_with('.') {
        return Err(ConfigError::new("baseUrl has an invalid host name"));
    }
    for label in host.split('.') {
        let valid = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !valid {
            return Err(ConfigError::new("baseUrl has an invalid host name"));
        }
    }
    Ok(host.to_string())
}

fn validate_name(field: &str, value: &str) -> Result<(), ConfigError> {
    let mut chars = value.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !first_ok || !rest_ok || value.len() > 64 {
        return Err(ConfigError::new(format!(
            "config: {field} must be 1-64 characters from A-Z a-z 0-9 . _ - and start alphanumeric"
        )));
    }
    Ok(())
}

fn validate_vpn_ip(text: &str) -> Result<(), ConfigError> {
    let ip = Ipv4Addr::from_str(text)
        .map_err(|_| ConfigError::new("config: vpn.ip must be an IPv4 address"))?;
    if !is_private_v4(ip) || ip.is_loopback() || ip.is_unspecified() {
        return Err(ConfigError::new(
            "config: vpn.ip must be a private or VPN address",
        ));
    }
    Ok(())
}

pub fn is_private_host(host: &str) -> bool {
    Ipv4Addr::from_str(host).map(is_private_v4).unwrap_or(false)
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    Ipv4Addr::from_str(host)
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
        || host == "::1"
}

/// RFC 1918, loopback, link-local, and the 100.64/10 shared range a VPN or
/// tailnet assigns.
fn is_private_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example(mode: &str, base_url: &str) -> String {
        format!(
            r#"{{
  "version": 1,
  "mode": "{mode}",
  "baseUrl": "{base_url}",
  "behindProxy": {},
  "port": 8810,
  "container": {{
    "name": "ntfy",
    "image": "localhost/ntfy:2.37.2",
    "cacheVolume": "ntfy-cache"
  }},
  "vpn": {{ "ip": "100.64.0.10" }}
}}"#,
            mode == "proxy"
        )
    }

    #[test]
    fn parses_the_vpn_example() {
        let config = DeployConfig::parse_text(&example("vpn", "http://100.64.0.10:8810")).unwrap();
        assert_eq!(config.mode, Mode::Vpn);
        assert_eq!(config.base_url().unwrap().host(), "100.64.0.10");
        assert_eq!(config.vpn_ip(), Some(Ipv4Addr::new(100, 64, 0, 10)));
        assert!(!config.behind_proxy);
    }

    #[test]
    fn rejects_an_unknown_mode() {
        let error = DeployConfig::parse_text(&example("mesh", "http://100.64.0.10:8810"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown variant"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_field() {
        let text = example("vpn", "http://100.64.0.10:8810").replace(
            "\"port\": 8810,",
            "\"port\": 8810, \"fallbackPath\": \"/x\",",
        );
        let error = DeployConfig::parse_text(&text).unwrap_err().to_string();
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_container_field() {
        let text = example("vpn", "http://100.64.0.10:8810").replace(
            "\"cacheVolume\": \"ntfy-cache\"",
            "\"cacheVolume\": \"ntfy-cache\", \"network\": \"host\"",
        );
        let error = DeployConfig::parse_text(&text).unwrap_err().to_string();
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn vpn_requires_an_ip() {
        let text = example("vpn", "http://100.64.0.10:8810")
            .replace(",\n  \"vpn\": { \"ip\": \"100.64.0.10\" }", "");
        assert!(DeployConfig::parse_text(&text).is_err());
        let config = DeployConfig::parse_text(&example("vpn", "http://100.64.0.10:8810")).unwrap();
        let mut missing = config.clone();
        missing.vpn = None;
        let error = missing.validate().unwrap_err().to_string();
        assert!(error.contains("vpn.ip is required"), "{error}");
    }

    #[test]
    fn https_is_accepted_for_any_host() {
        let config =
            DeployConfig::parse_text(&example("proxy", "https://ntfy.example.ts.net")).unwrap();
        assert_eq!(config.base_url().unwrap().scheme(), Scheme::Https);
        let lan =
            DeployConfig::parse_text(&example("lan", "https://ntfy.example.internal")).unwrap();
        assert_eq!(lan.mode, Mode::Lan);
    }

    #[test]
    fn http_is_only_accepted_on_loopback_vpn_or_lan() {
        let public = example("lan", "http://203.0.113.5:8810");
        let error = DeployConfig::parse_text(&public).unwrap_err().to_string();
        assert!(error.contains("loopback, VPN, or LAN"), "{error}");
        let config = DeployConfig::parse_text(&example("vpn", "http://100.64.0.10:8810")).unwrap();
        assert_eq!(config.mode, Mode::Vpn);
        let lan = DeployConfig::parse_text(&example("lan", "http://192.168.1.20:8810")).unwrap();
        assert_eq!(lan.mode, Mode::Lan);
    }

    #[test]
    fn http_is_refused_in_proxy_mode() {
        let error = DeployConfig::parse_text(&example("proxy", "http://127.0.0.1:8810"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("proxy mode requires an https"), "{error}");
    }

    #[test]
    fn url_rejects_paths_queries_and_trailing_slashes() {
        for bad in [
            "https://ntfy.example.ts.net/",
            "https://ntfy.example.ts.net/api",
            "https://ntfy.example.ts.net?x=1",
            "https://ntfy.example.ts.net#x",
            "https://user:pass@ntfy.example.ts.net",
            "https://ntfy example.net",
        ] {
            let error = BaseUrl::parse(bad).unwrap_err().to_string();
            assert!(!error.is_empty(), "{bad}");
        }
    }

    #[test]
    fn http_url_port_must_match_the_deploy_port() {
        let error = DeployConfig::parse_text(&example("vpn", "http://100.64.0.10:8811"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("must match port 8810"), "{error}");
    }

    #[test]
    fn behind_proxy_is_true_only_for_proxy_mode() {
        let text = example("vpn", "http://100.64.0.10:8810")
            .replace("\"behindProxy\": false", "\"behindProxy\": true");
        let error = DeployConfig::parse_text(&text).unwrap_err().to_string();
        assert!(error.contains("behindProxy is true only"), "{error}");
    }

    #[test]
    fn vpn_ip_must_be_private_and_not_loopback() {
        for bad in ["127.0.0.1", "203.0.113.9", "not-an-ip"] {
            let text = example("vpn", "http://100.64.0.10:8810")
                .replace("\"100.64.0.10\"", &format!("\"{bad}\""));
            assert!(DeployConfig::parse_text(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn version_and_port_are_strict() {
        let version =
            example("vpn", "http://100.64.0.10:8810").replace("\"version\": 1", "\"version\": 2");
        assert!(DeployConfig::parse_text(&version).is_err());
        let port =
            example("vpn", "http://100.64.0.10:8810").replace("\"port\": 8810", "\"port\": 0");
        assert!(DeployConfig::parse_text(&port).is_err());
    }

    #[test]
    fn container_fields_are_strict() {
        let name = example("vpn", "http://100.64.0.10:8810")
            .replace("\"name\": \"ntfy\"", "\"name\": \"-ntfy\"");
        assert!(DeployConfig::parse_text(&name).is_err());
        let image = example("vpn", "http://100.64.0.10:8810")
            .replace("localhost/ntfy:2.37.2", "localhost/ntfy 2");
        assert!(DeployConfig::parse_text(&image).is_err());
    }

    #[test]
    fn ipv6_base_urls_are_parsed_with_brackets() {
        let url = BaseUrl::parse("http://[::1]:8810").unwrap();
        assert_eq!(url.host(), "::1");
        assert_eq!(url.port(), Some(8810));
        assert!(url.is_loopback());
        assert!(BaseUrl::parse("http://::1:8810").is_err());
    }

    #[test]
    fn install_paths_never_name_the_opencode_config_tree() {
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        // Built at run time so this assertion does not match its own source.
        let config_trees = [
            format!(".config{}opencode", '/'),
            format!(".config{}opencode", '\\'),
        ];
        let state_trees = [
            format!(".local{}state{}opencode", '/', '/'),
            format!(".local{}state{}opencode", '\\', '\\'),
        ];
        let mut checked = 0;
        for entry in walk(crate_dir) {
            let text = fs::read_to_string(&entry).unwrap_or_default();
            for forbidden in config_trees.iter().chain(state_trees.iter()) {
                assert!(!text.contains(forbidden), "{}", entry.display());
            }
            checked += 1;
        }
        assert!(checked > 0);
        let example = crate_dir
            .join("..")
            .join("..")
            .join("..")
            .join("relay-windows")
            .join("ntfy-deploy.example.json");
        if example.exists() {
            let text = fs::read_to_string(&example).unwrap();
            for forbidden in config_trees.iter().chain(state_trees.iter()) {
                assert!(!text.contains(forbidden));
            }
        }
    }

    fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(next) = stack.pop() {
            let entries = match fs::read_dir(&next) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    found.push(path);
                }
            }
        }
        found
    }
}
