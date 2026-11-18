//! Platform rules: the podman publish argument and the Windows portproxy.
//!
//! Pure functions only. The binary decides which platform it runs on and the
//! command runner performs the result; tests cover both platforms on any host.

use std::fmt;
use std::net::Ipv4Addr;

use crate::config::{ConfigError, DeployConfig, Mode};

/// Port ntfy listens on inside the container.
pub const CONTAINER_PORT: u16 = 8080;
/// Windows loopback bridge listener used by the TLS terminator and the relay.
pub const LOOPBACK_ADDRESS: &str = "127.0.0.1";
/// IPv6 target inside WSL; the Windows portproxy forwards to it.
pub const WSL_TARGET_ADDRESS: &str = "::1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Linux,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Linux => "linux",
        }
    }
}

/// One executable command. The program is a literal name, never a shell line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformCommand {
    pub program: &'static str,
    pub args: Vec<String>,
}

impl fmt::Display for PlatformCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.program)?;
        for arg in &self.args {
            write!(f, " {arg}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortproxyEntry {
    pub listen_address: String,
    pub listen_port: u16,
    pub connect_address: String,
    pub connect_port: u16,
}

impl fmt::Display for PortproxyEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{} -> {}:{}",
            self.listen_address, self.listen_port, self.connect_address, self.connect_port
        )
    }
}

/// The podman `-p` value. Windows always publishes inside WSL; the Windows
/// portproxy, not the container binding, is the exposure boundary. Linux
/// selects the address that the mode needs.
pub fn publish_spec(platform: Platform, config: &DeployConfig) -> Result<String, ConfigError> {
    match platform {
        Platform::Windows => Ok(format!("{}:{CONTAINER_PORT}", config.port)),
        Platform::Linux => {
            match config.mode {
                // The relay always reaches ntfy through loopback; proxy mode
                // therefore binds loopback only.
                Mode::Proxy => Ok(format!(
                    "{LOOPBACK_ADDRESS}:{}:{CONTAINER_PORT}",
                    config.port
                )),
                // vpn and lan publish all interfaces so loopback health and the
                // relay stay reachable in every mode. Deny-all auth and the host
                // firewall bound the exposure.
                Mode::Vpn | Mode::Lan => Ok(format!("{}:{CONTAINER_PORT}", config.port)),
            }
        }
    }
}

/// Portproxy rules that must exist for the mode. The loopback bridge stays in
/// every mode; only `vpn` adds the VPN listener.
pub fn expected_portproxy(config: &DeployConfig) -> Vec<PortproxyEntry> {
    let mut entries = vec![PortproxyEntry {
        listen_address: LOOPBACK_ADDRESS.to_string(),
        listen_port: config.port,
        connect_address: WSL_TARGET_ADDRESS.to_string(),
        connect_port: config.port,
    }];
    if config.mode == Mode::Vpn {
        if let Some(vpn) = config.vpn_ip() {
            entries.push(PortproxyEntry {
                listen_address: vpn.to_string(),
                listen_port: config.port,
                connect_address: WSL_TARGET_ADDRESS.to_string(),
                connect_port: config.port,
            });
        }
    }
    entries
}

/// Listen addresses this tool owns. A stale VPN listener is removed even when
/// the config no longer names it.
pub fn managed_addresses(config: &DeployConfig) -> Vec<String> {
    let mut addresses = vec![LOOPBACK_ADDRESS.to_string()];
    if let Some(vpn) = config.vpn_ip() {
        addresses.push(vpn.to_string());
    }
    addresses
}

/// Parses `netsh interface portproxy show v4tov6`. Header and separator lines
/// are skipped; a row is four whitespace-separated fields that start with an
/// IPv4 listener address and two valid ports.
pub fn parse_portproxy_show(text: &str) -> Vec<PortproxyEntry> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 4 {
            continue;
        }
        if fields[0].parse::<Ipv4Addr>().is_err() {
            continue;
        }
        let (Ok(listen_port), Ok(connect_port)) =
            (fields[1].parse::<u16>(), fields[3].parse::<u16>())
        else {
            continue;
        };
        entries.push(PortproxyEntry {
            listen_address: fields[0].to_string(),
            listen_port,
            connect_address: fields[2].to_string(),
            connect_port,
        });
    }
    entries
}

/// Commands that turn `current` into `expected` for the addresses this tool
/// manages. Deletions run before additions. Unrelated rows are left alone.
pub fn portproxy_commands(
    current: &[PortproxyEntry],
    expected: &[PortproxyEntry],
    managed: &[String],
    port: u16,
) -> Vec<PlatformCommand> {
    let mut commands = Vec::new();
    for entry in current {
        if entry.listen_port != port || !managed.iter().any(|a| a == &entry.listen_address) {
            continue;
        }
        if !expected.contains(entry) {
            commands.push(netsh_delete(entry));
        }
    }
    for entry in expected {
        if !current.contains(entry) {
            commands.push(netsh_add(entry));
        }
    }
    commands
}

pub fn netsh_show() -> PlatformCommand {
    PlatformCommand {
        program: "netsh",
        args: vec![
            "interface".to_string(),
            "portproxy".to_string(),
            "show".to_string(),
            "v4tov6".to_string(),
        ],
    }
}

fn netsh_delete(entry: &PortproxyEntry) -> PlatformCommand {
    PlatformCommand {
        program: "netsh",
        args: vec![
            "interface".to_string(),
            "portproxy".to_string(),
            "delete".to_string(),
            "v4tov6".to_string(),
            format!("listenaddress={}", entry.listen_address),
            format!("listenport={}", entry.listen_port),
        ],
    }
}

fn netsh_add(entry: &PortproxyEntry) -> PlatformCommand {
    PlatformCommand {
        program: "netsh",
        args: vec![
            "interface".to_string(),
            "portproxy".to_string(),
            "add".to_string(),
            "v4tov6".to_string(),
            format!("listenaddress={}", entry.listen_address),
            format!("listenport={}", entry.listen_port),
            format!("connectaddress={}", entry.connect_address),
            format!("connectport={}", entry.connect_port),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeployConfig;

    const SHOW: &str = "
Listen on ipv4:             Connect to ipv6:

Address         Port        Address         Port
--------------- ----------  --------------- ----------
127.0.0.1       8810        ::1             8810
100.64.0.10   8810        ::1             8810
";

    fn config(mode: &str, base_url: &str) -> DeployConfig {
        let data = format!(
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
        );
        DeployConfig::parse_text(&data).unwrap()
    }

    #[test]
    fn windows_always_publishes_the_container_port() {
        for mode in ["proxy", "vpn", "lan"] {
            let base = match mode {
                "proxy" => "https://ntfy.example.ts.net",
                "vpn" => "http://100.64.0.10:8810",
                _ => "http://192.168.1.20:8810",
            };
            let config = config(mode, base);
            assert_eq!(
                publish_spec(Platform::Windows, &config).unwrap(),
                "8810:8080"
            );
        }
    }

    #[test]
    fn linux_publish_follows_the_mode() {
        let proxy = config("proxy", "https://ntfy.example.ts.net");
        assert_eq!(
            publish_spec(Platform::Linux, &proxy).unwrap(),
            "127.0.0.1:8810:8080"
        );
        let vpn = config("vpn", "http://100.64.0.10:8810");
        assert_eq!(publish_spec(Platform::Linux, &vpn).unwrap(), "8810:8080");
        let lan = config("lan", "http://192.168.1.20:8810");
        assert_eq!(publish_spec(Platform::Linux, &lan).unwrap(), "8810:8080");
    }

    #[test]
    fn expected_rules_follow_the_mode() {
        let proxy = config("proxy", "https://ntfy.example.ts.net");
        let expected = expected_portproxy(&proxy);
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0].listen_address, "127.0.0.1");

        let vpn = config("vpn", "http://100.64.0.10:8810");
        let expected = expected_portproxy(&vpn);
        assert_eq!(expected.len(), 2);
        assert_eq!(expected[1].listen_address, "100.64.0.10");

        let lan = config("lan", "http://192.168.1.20:8810");
        assert_eq!(expected_portproxy(&lan).len(), 1);
    }

    #[test]
    fn show_output_parses_rows_and_skips_headers() {
        let entries = parse_portproxy_show(SHOW);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].listen_address, "127.0.0.1");
        assert_eq!(entries[1].listen_address, "100.64.0.10");
        assert_eq!(entries[1].connect_address, "::1");
    }

    #[test]
    fn proxy_removes_the_vpn_entry() {
        let proxy = config("proxy", "https://ntfy.example.ts.net");
        let commands = portproxy_commands(
            &parse_portproxy_show(SHOW),
            &expected_portproxy(&proxy),
            &managed_addresses(&proxy),
            proxy.port,
        );
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].args[2], "delete");
        assert!(commands[0]
            .args
            .contains(&"listenaddress=100.64.0.10".to_string()));
    }

    #[test]
    fn vpn_adds_the_vpn_entry() {
        let vpn = config("vpn", "http://100.64.0.10:8810");
        let commands = portproxy_commands(
            &parse_portproxy_show(""),
            &expected_portproxy(&vpn),
            &managed_addresses(&vpn),
            vpn.port,
        );
        assert_eq!(commands.len(), 2);
        assert!(commands.iter().all(|command| command.args[2] == "add"));
        assert!(commands.iter().any(|command| command
            .args
            .contains(&"listenaddress=100.64.0.10".to_string())));
    }

    #[test]
    fn lan_keeps_only_the_loopback_entry() {
        let lan = config("lan", "http://192.168.1.20:8810");
        let commands = portproxy_commands(
            &parse_portproxy_show(SHOW),
            &expected_portproxy(&lan),
            &managed_addresses(&lan),
            lan.port,
        );
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].args[2], "delete");
    }

    #[test]
    fn a_matching_table_produces_no_commands() {
        let vpn = config("vpn", "http://100.64.0.10:8810");
        let commands = portproxy_commands(
            &parse_portproxy_show(SHOW),
            &expected_portproxy(&vpn),
            &managed_addresses(&vpn),
            vpn.port,
        );
        assert!(commands.is_empty());
    }

    #[test]
    fn a_wrong_target_is_replaced() {
        let text = "127.0.0.1 8810 127.0.0.1 8810";
        let proxy = config("proxy", "https://ntfy.example.ts.net");
        let commands = portproxy_commands(
            &parse_portproxy_show(text),
            &expected_portproxy(&proxy),
            &managed_addresses(&proxy),
            proxy.port,
        );
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].args[2], "delete");
        assert_eq!(commands[1].args[2], "add");
    }
}
