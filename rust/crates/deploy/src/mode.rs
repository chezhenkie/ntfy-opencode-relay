//! `ntfy-deploy mode`: detect the available network paths and write the deploy
//! config for the chosen one.
//!
//! Detection reads interface addresses from the platform tools already used by
//! the crate (`netsh` on Windows, `ip` on Linux) and the Tailscale status JSON.
//! The tool writes only `ntfy-deploy.json`; the relay, its config, and its token
//! are never touched.

use std::net::Ipv4Addr;
use std::path::Path;
use std::str::FromStr;

use serde_json::Value;

use crate::config::{ConfigError, ContainerConfig, DeployConfig, Mode, VpnConfig};
use crate::deploy::DeployError;
use crate::platform::Platform;
use crate::runner::CommandRunner;

/// Paths an operator can choose. The id is the command-line name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: &'static str,
    pub title: &'static str,
    pub detail: String,
    pub mode: Mode,
    pub base_url: String,
    pub vpn_ip: Option<String>,
    pub note: Option<String>,
}

impl Choice {
    pub fn line(&self, number: usize) -> String {
        let body = format!("{number}) {:<9} {}", self.title, self.detail);
        match &self.note {
            Some(note) => format!("{body}\n     note: {note}"),
            None => body,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub ip: Option<Ipv4Addr>,
    pub gateway: Option<Ipv4Addr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscaleStatus {
    pub dns_name: String,
    pub ip: Option<Ipv4Addr>,
    pub running: bool,
}

/// Interface blocks printed by `netsh interface ipv4 show addresses`.
pub fn parse_netsh_addresses(text: &str) -> Vec<Interface> {
    let mut out = Vec::new();
    let mut current: Option<Interface> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Configuration for interface ") {
            if let Some(done) = current.take() {
                out.push(done);
            }
            current = Some(Interface {
                name: rest.trim().trim_matches('"').to_string(),
                ip: None,
                gateway: None,
            });
        } else if let Some(value) = trimmed.strip_prefix("IP Address:") {
            if let Some(iface) = current.as_mut() {
                iface.ip = Ipv4Addr::from_str(value.trim()).ok();
            }
        } else if let Some(value) = trimmed.strip_prefix("Default Gateway:") {
            if let Some(iface) = current.as_mut() {
                iface.gateway = Ipv4Addr::from_str(value.trim()).ok();
            }
        }
    }
    if let Some(done) = current {
        out.push(done);
    }
    out
}

/// Interface rows printed by `ip -4 -o addr show`.
pub fn parse_ip_addr(text: &str) -> Vec<Interface> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(index) = parts.next() else { continue };
        if !index.ends_with(':') {
            continue;
        }
        let Some(name) = parts.next() else { continue };
        let name = name.split('@').next().unwrap_or(name);
        let mut address = None;
        while let Some(token) = parts.next() {
            if token == "inet" {
                address = parts
                    .next()
                    .and_then(|cidr| cidr.split('/').next())
                    .and_then(|ip| Ipv4Addr::from_str(ip).ok());
                break;
            }
        }
        if let Some(ip) = address {
            out.push(Interface {
                name: name.to_string(),
                ip: Some(ip),
                gateway: None,
            });
        }
    }
    out
}

/// Device named by `ip route show default`.
pub fn parse_default_route(text: &str) -> Option<String> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    tokens
        .windows(2)
        .find(|pair| pair[0] == "dev")
        .map(|pair| pair[1].to_string())
}

/// `tailscale status --json`: the node's MagicDNS name and IPv4.
pub fn parse_tailscale_status(text: &str) -> Option<TailscaleStatus> {
    let value: Value = serde_json::from_str(text).ok()?;
    let node = value.get("Self")?;
    let dns_name = node
        .get("DNSName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end_matches('.')
        .to_string();
    let ip = node
        .get("TailscaleIPs")
        .and_then(Value::as_array)
        .and_then(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .find_map(|value| Ipv4Addr::from_str(value).ok())
        });
    let running = value
        .get("BackendState")
        .and_then(Value::as_str)
        .is_some_and(|state| state == "Running");
    Some(TailscaleStatus {
        dns_name,
        ip,
        running,
    })
}

fn is_shared_range(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

fn is_vpn_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("tailscale") || lower.contains("nord") || lower.contains("meshnet")
}

fn is_virtual_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("loopback")
        || lower.contains("vethernet")
        || lower.contains("wsl")
        || lower.contains("bluetooth")
        || lower.contains("virtual")
}

fn usable_lan_ip(iface: &Interface) -> Option<Ipv4Addr> {
    iface.ip.filter(|ip| {
        (ip.is_private() || is_shared_range(*ip)) && !ip.is_loopback() && !ip.is_link_local()
    })
}

/// Builds the menu from detection results. A path is offered only when its
/// address was found, so the list never shows an entry that cannot work.
pub fn build_choices(
    interfaces: &[Interface],
    tailscale: Option<&TailscaleStatus>,
    default_iface: Option<&str>,
    port: u16,
    current: &DeployConfig,
) -> Vec<Choice> {
    let mut choices = Vec::new();

    let tailscale_iface = interfaces
        .iter()
        .find(|iface| iface.name.eq_ignore_ascii_case("Tailscale"));
    let tailscale_ip = tailscale
        .and_then(|status| status.ip)
        .or_else(|| tailscale_iface.and_then(|iface| iface.ip));
    if let Some(status) = tailscale.filter(|status| status.running && !status.dns_name.is_empty()) {
        if let Some(ip) = tailscale_ip {
            choices.push(Choice {
                id: "tailscale",
                title: "Tailscale",
                detail: format!("proxy  https://{}", status.dns_name),
                mode: Mode::Proxy,
                base_url: format!("https://{}", status.dns_name),
                vpn_ip: Some(ip.to_string()),
                note: meshnet_note(interfaces),
            });
        }
    }

    if let Some((iface, ip)) = nord_address(interfaces) {
        let tailscale_up = choices.iter().any(|choice| choice.id == "tailscale");
        let note = if is_shared_range(ip) {
            None
        } else {
            Some(format!(
                "{} looks like the Nord tunnel address; Meshnet peers normally use 100.64.x.x",
                iface
            ))
        };
        let note = match (note, tailscale_up) {
            (Some(note), true) => Some(format!(
                "{note}; Tailscale is up and both use 100.64/10, disable one of them"
            )),
            (None, true) => Some(
                "Tailscale is up and both use 100.64/10, disable Tailscale while Meshnet is active"
                    .to_string(),
            ),
            (note, false) => note,
        };
        choices.push(Choice {
            id: "nord",
            title: "NordVPN",
            detail: format!("vpn    http://{ip}:{port}"),
            mode: Mode::Vpn,
            base_url: format!("http://{ip}:{port}"),
            vpn_ip: Some(ip.to_string()),
            note,
        });
    }

    let lan_iface = interfaces.iter().find(|iface| match default_iface {
        Some(name) => iface.name == name,
        None => iface.gateway.is_some(),
    });
    if let Some(ip) = lan_iface
        .filter(|iface| !is_vpn_name(&iface.name) && !is_virtual_name(&iface.name))
        .and_then(usable_lan_ip)
    {
        choices.push(Choice {
            id: "lan",
            title: "Local network",
            detail: format!("lan    http://{ip}:{port}"),
            mode: Mode::Lan,
            base_url: format!("http://{ip}:{port}"),
            vpn_ip: current.vpn.as_ref().map(|vpn| vpn.ip.clone()),
            note: Some("the phone must be on the same network".to_string()),
        });
    }

    choices
}

fn meshnet_note(interfaces: &[Interface]) -> Option<String> {
    nord_address(interfaces).map(|(_, ip)| {
        format!("Nord meshnet address {ip} is also present, disable Meshnet while Tailscale routes")
    })
}

/// The Nord address: a private IPv4 on an interface whose name names Nord or
/// Meshnet, preferring the 100.64/10 range Meshnet peers use.
fn nord_address(interfaces: &[Interface]) -> Option<(String, Ipv4Addr)> {
    let mut candidates: Vec<(String, Ipv4Addr)> = interfaces
        .iter()
        .filter(|iface| {
            let lower = iface.name.to_ascii_lowercase();
            lower.contains("nord") || lower.contains("meshnet")
        })
        .filter_map(|iface| usable_lan_ip(iface).map(|ip| (iface.name.clone(), ip)))
        .collect();
    candidates.sort_by_key(|(_, ip)| !is_shared_range(*ip));
    candidates.into_iter().next()
}

/// The deploy config for one choice. Container settings and the port are kept.
pub fn config_from_choice(current: &DeployConfig, choice: &Choice) -> DeployConfig {
    DeployConfig {
        version: 1,
        mode: choice.mode,
        base_url: choice.base_url.clone(),
        behind_proxy: choice.mode == Mode::Proxy,
        port: current.port,
        container: ContainerConfig {
            name: current.container.name.clone(),
            image: current.container.image.clone(),
            cache_volume: current.container.cache_volume.clone(),
        },
        vpn: choice
            .vpn_ip
            .as_ref()
            .map(|ip| VpnConfig { ip: ip.clone() }),
    }
}

/// Writes the config, after proving the exact text parses and validates.
pub fn write_config(path: &Path, config: &DeployConfig) -> Result<(), ConfigError> {
    let mut text = config.to_json()?;
    text.push('\n');
    DeployConfig::parse_text(&text)?;
    std::fs::write(path, text).map_err(|error| {
        ConfigError::new(format!("config: cannot write {}: {error}", path.display()))
    })
}

/// Detects the choices on this host. A missing tool or a stopped VPN only
/// removes its entry; it is not an error.
pub fn detect(
    runner: &dyn CommandRunner,
    platform: Platform,
    port: u16,
    current: &DeployConfig,
) -> Result<Vec<Choice>, DeployError> {
    let (interfaces, default_iface) = match platform {
        Platform::Windows => {
            let args = vec![
                "interface".to_string(),
                "ipv4".to_string(),
                "show".to_string(),
                "addresses".to_string(),
            ];
            let output = runner.run("netsh", &args)?;
            let interfaces = if output.status == 0 {
                parse_netsh_addresses(&output.stdout)
            } else {
                Vec::new()
            };
            (interfaces, None)
        }
        Platform::Linux => {
            let args = vec![
                "-4".to_string(),
                "-o".to_string(),
                "addr".to_string(),
                "show".to_string(),
            ];
            let output = runner.run("ip", &args)?;
            let interfaces = if output.status == 0 {
                parse_ip_addr(&output.stdout)
            } else {
                Vec::new()
            };
            let route = runner.run(
                "ip",
                &[
                    "route".to_string(),
                    "show".to_string(),
                    "default".to_string(),
                ],
            )?;
            let default_iface = (route.status == 0)
                .then(|| parse_default_route(&route.stdout))
                .flatten();
            (interfaces, default_iface)
        }
    };

    let tailscale = tailscale_status(runner, platform);
    Ok(build_choices(
        &interfaces,
        tailscale.as_ref(),
        default_iface.as_deref(),
        port,
        current,
    ))
}

fn tailscale_status(runner: &dyn CommandRunner, platform: Platform) -> Option<TailscaleStatus> {
    let args = vec!["status".to_string(), "--json".to_string()];
    let mut candidates: Vec<&str> = vec!["tailscale"];
    match platform {
        Platform::Windows => candidates.push(r"C:\Program Files\Tailscale\tailscale.exe"),
        Platform::Linux => candidates.extend(["/usr/bin/tailscale", "/usr/local/bin/tailscale"]),
    }
    for program in candidates {
        if let Ok(output) = runner.run(program, &args) {
            if output.status == 0 {
                if let Some(status) = parse_tailscale_status(&output.stdout) {
                    return Some(status);
                }
            }
        }
    }
    None
}

/// Resolves a command-line name against the detected choices.
pub fn find_choice<'a>(choices: &'a [Choice], id: &str) -> Option<&'a Choice> {
    choices.iter().find(|choice| choice.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NETSH: &str = "
Configuration for interface \"Tailscale\"
    DHCP enabled:                         No
    IP Address:                           100.64.0.10
    Subnet Prefix:                        100.64.0.10/32 (mask 255.255.255.255)

Configuration for interface \"NordLynx\"
    DHCP enabled:                         No
    IP Address:                           10.5.0.2

Configuration for interface \"Wi-Fi\"
    DHCP enabled:                         Yes
    IP Address:                           192.168.2.9
    Default Gateway:                      192.168.2.254
";

    const IP_ADDR: &str = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever
2: wlan0    inet 192.168.2.9/24 brd 192.168.2.255 scope global dynamic wlan0\\       valid_lft 86390sec
3: tailscale0    inet 100.64.0.10/32 scope global tailscale0\\       valid_lft forever
";

    fn sample_config() -> DeployConfig {
        DeployConfig::parse_text(
            r#"{
  "version": 1,
  "mode": "proxy",
  "baseUrl": "https://ntfy.example.ts.net",
  "behindProxy": true,
  "port": 8810,
  "container": { "name": "ntfy", "image": "localhost/ntfy:server", "cacheVolume": "ntfy-cache" },
  "vpn": { "ip": "100.64.0.10" }
}"#,
        )
        .unwrap()
    }

    fn status(running: bool) -> TailscaleStatus {
        TailscaleStatus {
            dns_name: "ntfy.example.ts.net".to_string(),
            ip: Ipv4Addr::from_str("100.64.0.10").ok(),
            running,
        }
    }

    #[test]
    fn netsh_blocks_parse_to_interfaces() {
        let interfaces = parse_netsh_addresses(NETSH);
        assert_eq!(interfaces.len(), 3);
        assert_eq!(interfaces[0].name, "Tailscale");
        assert_eq!(
            interfaces[0].ip,
            Some(Ipv4Addr::from_str("100.64.0.10").unwrap())
        );
        assert_eq!(
            interfaces[2].gateway,
            Some(Ipv4Addr::from_str("192.168.2.254").unwrap())
        );
    }

    #[test]
    fn ip_addr_rows_parse_with_the_interface_name() {
        let interfaces = parse_ip_addr(IP_ADDR);
        assert_eq!(interfaces.len(), 3);
        assert_eq!(interfaces[1].name, "wlan0");
        assert_eq!(
            interfaces[1].ip,
            Some(Ipv4Addr::from_str("192.168.2.9").unwrap())
        );
    }

    #[test]
    fn default_route_names_the_device() {
        let text = "default via 192.168.2.254 dev wlan0 proto dhcp src 192.168.2.9 metric 600";
        assert_eq!(parse_default_route(text), Some("wlan0".to_string()));
        assert_eq!(parse_default_route("nonsense"), None);
    }

    #[test]
    fn tailscale_status_reads_the_dns_name_and_ipv4() {
        let text = r#"{
  "BackendState": "Running",
  "Self": {
    "DNSName": "ntfy.example.ts.net.",
    "TailscaleIPs": ["100.64.0.10", "2001:db8::1"]
  }
}"#;
        let status = parse_tailscale_status(text).unwrap();
        assert_eq!(status.dns_name, "ntfy.example.ts.net");
        assert_eq!(status.ip, Some(Ipv4Addr::from_str("100.64.0.10").unwrap()));
        assert!(status.running);
    }

    #[test]
    fn a_running_tailscale_and_wifi_offer_two_choices() {
        let config = sample_config();
        let interfaces = parse_netsh_addresses(NETSH);
        let choices = build_choices(&interfaces, Some(&status(true)), None, 8810, &config);
        let ids: Vec<_> = choices.iter().map(|choice| choice.id).collect();
        assert!(ids.contains(&"tailscale"));
        assert!(ids.contains(&"lan"));
        let tailscale = find_choice(&choices, "tailscale").unwrap();
        assert_eq!(tailscale.base_url, "https://ntfy.example.ts.net");
        assert_eq!(tailscale.mode, Mode::Proxy);
        let lan = find_choice(&choices, "lan").unwrap();
        assert_eq!(lan.base_url, "http://192.168.2.9:8810");
        assert_eq!(lan.mode, Mode::Lan);
    }

    #[test]
    fn nord_appears_with_its_meshnet_address() {
        let mesh = ["100", "64", "0", "20"].join(".");
        let text = NETSH.replace("10.5.0.2", &mesh);
        let interfaces = parse_netsh_addresses(&text);
        let config = sample_config();
        let choices = build_choices(&interfaces, None, None, 8810, &config);
        let nord = find_choice(&choices, "nord").unwrap();
        assert_eq!(nord.base_url, format!("http://{}:8810", mesh));
        assert_eq!(nord.mode, Mode::Vpn);
        assert_eq!(nord.vpn_ip.as_deref(), Some(mesh.as_str()));
        assert!(nord.note.is_none());
    }

    #[test]
    fn a_stopped_tailscale_is_not_offered() {
        let config = sample_config();
        let interfaces = parse_netsh_addresses(NETSH);
        let choices = build_choices(&interfaces, Some(&status(false)), None, 8810, &config);
        assert!(find_choice(&choices, "tailscale").is_none());
    }

    #[test]
    fn a_tunnel_only_nord_address_carries_a_note() {
        let config = sample_config();
        let interfaces = parse_netsh_addresses(NETSH);
        let choices = build_choices(&interfaces, None, None, 8810, &config);
        let nord = find_choice(&choices, "nord").unwrap();
        assert_eq!(nord.base_url, "http://10.5.0.2:8810");
        assert!(nord.note.as_ref().unwrap().contains("100.64"));
    }

    #[test]
    fn a_choice_becomes_a_valid_config() {
        let config = sample_config();
        let mesh = ["100", "64", "0", "20"].join(".");
        let interfaces = parse_netsh_addresses(&NETSH.replace("10.5.0.2", &mesh));
        let choices = build_choices(&interfaces, Some(&status(true)), None, 8810, &config);
        for choice in &choices {
            let next = config_from_choice(&config, choice);
            let text = next.to_json().unwrap();
            let parsed = DeployConfig::parse_text(&text).unwrap();
            assert_eq!(parsed.mode, choice.mode);
            assert_eq!(parsed.base_url, choice.base_url);
            assert_eq!(parsed.container.name, "ntfy");
            assert_eq!(parsed.port, 8810);
        }
    }

    #[test]
    fn the_written_config_keeps_the_example_shape() {
        let config = sample_config();
        let interfaces = parse_netsh_addresses(NETSH);
        let choices = build_choices(&interfaces, Some(&status(true)), None, 8810, &config);
        let next = config_from_choice(&config, find_choice(&choices, "lan").unwrap());
        let text = next.to_json().unwrap();
        assert!(text.contains("\"baseUrl\": \"http://192.168.2.9:8810\""));
        assert!(text.contains("\"behindProxy\": false"));
        assert!(text.contains("\"cacheVolume\": \"ntfy-cache\""));
        assert!(!text.contains("\"vpn\"") || text.contains("\"ip\""));
    }
}
