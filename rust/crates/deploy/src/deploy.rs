//! Apply, idempotency, health probing, and the plan shared with `status`.
//!
//! The container is replaced only when it is missing, unhealthy, or was
//! started with different arguments. The Windows loopback bridge is ensured
//! before the health wait because the relay and the terminator use it; the VPN
//! listener is reconciled after health.

use std::fmt;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::config::{ConfigError, DeployConfig, Mode};
use crate::platform::{
    expected_portproxy, managed_addresses, netsh_show, parse_portproxy_show, portproxy_commands,
    publish_spec, Platform, PlatformCommand, PortproxyEntry, LOOPBACK_ADDRESS,
};

const PODMAN: &str = "podman";
const INSPECT_PORT_KEY: &str = "8080/tcp";

#[derive(Debug)]
pub enum DeployError {
    Config(ConfigError),
    Unhealthy { port: u16, detail: String },
    Platform { program: String, detail: String },
    Io { detail: String },
}

impl DeployError {
    pub fn platform(program: &str, detail: impl Into<String>) -> Self {
        Self::Platform {
            program: program.to_string(),
            detail: detail.into(),
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Config(_) | Self::Io { .. } => 2,
            Self::Unhealthy { .. } => 3,
            Self::Platform { .. } => 4,
        }
    }
}

impl fmt::Display for DeployError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => f.write_str(&error.to_string()),
            Self::Unhealthy { port, detail } => {
                write!(f, "health: no 200 on 127.0.0.1:{port}/v1/health: {detail}")
            }
            Self::Platform { program, detail } => write!(f, "{program}: {detail}"),
            Self::Io { detail } => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for DeployError {}

impl From<ConfigError> for DeployError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthRetry {
    pub attempts: u32,
    pub interval: Duration,
}

impl Default for HealthRetry {
    fn default() -> Self {
        Self {
            attempts: 30,
            interval: Duration::from_secs(1),
        }
    }
}

pub trait HealthProbe {
    fn get(&self, port: u16) -> Result<u16, String>;
}

/// One plain HTTP/1.1 GET to the loopback health endpoint. No TLS and no
/// redirects; `/v1/health` is anonymous.
pub struct TcpHealth {
    pub timeout: Duration,
}

impl TcpHealth {
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(2),
        }
    }
}

impl Default for TcpHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthProbe for TcpHealth {
    fn get(&self, port: u16) -> Result<u16, String> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let mut stream = TcpStream::connect_timeout(&address, self.timeout)
            .map_err(|error| format!("connect: {error}"))?;
        let _ = stream.set_read_timeout(Some(self.timeout));
        let _ = stream.set_write_timeout(Some(self.timeout));
        let request = format!(
            "GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("write: {error}"))?;
        let mut buffer = [0u8; 1024];
        let read = stream
            .read(&mut buffer)
            .map_err(|error| format!("read: {error}"))?;
        let head = String::from_utf8_lossy(&buffer[..read]);
        parse_status_line(&head).ok_or_else(|| "no HTTP status line".to_string())
    }
}

fn parse_status_line(head: &str) -> Option<u16> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse::<u16>().ok()
}

#[derive(Debug, Clone)]
pub struct PlanSummary {
    pub platform: Platform,
    pub mode: Mode,
    pub base_url: String,
    pub container: String,
    pub image: String,
    pub publish: String,
    pub env: [String; 2],
    pub portproxy: Vec<PortproxyEntry>,
}

/// Validated values for one apply or status run.
pub struct Plan<'a> {
    pub config: &'a DeployConfig,
    pub platform: Platform,
    pub publish: String,
    pub env: [String; 2],
    pub portproxy: Vec<PortproxyEntry>,
    pub managed: Vec<String>,
}

impl<'a> Plan<'a> {
    pub fn new(config: &'a DeployConfig, platform: Platform) -> Result<Self, ConfigError> {
        config.validate()?;
        config.base_url()?;
        let publish = publish_spec(platform, config)?;
        let env = [
            format!("NTFY_BASE_URL={}", config.base_url),
            format!("NTFY_BEHIND_PROXY={}", config.behind_proxy),
        ];
        Ok(Self {
            config,
            platform,
            publish,
            env,
            portproxy: expected_portproxy(config),
            managed: managed_addresses(config),
        })
    }

    pub fn summary(&self) -> PlanSummary {
        PlanSummary {
            platform: self.platform,
            mode: self.config.mode,
            base_url: self.config.base_url.clone(),
            container: self.config.container.name.clone(),
            image: self.config.container.image.clone(),
            publish: self.publish.clone(),
            env: self.env.clone(),
            portproxy: self.portproxy.clone(),
        }
    }

    pub fn health_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1/health", self.config.port)
    }

    /// The host a matching container must bind. Windows publishes inside the
    /// WSL VM, so its container is a wildcard in every mode; on Linux only
    /// proxy mode binds loopback, and the other modes bind all interfaces.
    pub(crate) fn host_allowed(&self, host: &str) -> bool {
        let host = host.trim();
        let wildcard = host.is_empty() || host == "0.0.0.0" || host == "::";
        match (self.platform, self.config.mode) {
            (Platform::Linux, Mode::Proxy) => host == LOOPBACK_ADDRESS || host == "::1",
            _ => wildcard,
        }
    }

    pub fn inspect_command(&self) -> PlatformCommand {
        PlatformCommand {
            program: PODMAN,
            args: vec!["inspect".to_string(), self.config.container.name.clone()],
        }
    }

    /// The image ID behind the configured reference. A same-tag rebuild keeps
    /// the reference but changes this ID, so reuse has to compare it.
    pub fn image_inspect_command(&self) -> PlatformCommand {
        PlatformCommand {
            program: PODMAN,
            args: vec![
                "image".to_string(),
                "inspect".to_string(),
                "--format".to_string(),
                "{{.Id}}".to_string(),
                self.config.container.image.clone(),
            ],
        }
    }

    pub fn stop_command(&self) -> PlatformCommand {
        PlatformCommand {
            program: PODMAN,
            args: vec!["stop".to_string(), self.config.container.name.clone()],
        }
    }

    pub fn remove_command(&self) -> PlatformCommand {
        PlatformCommand {
            program: PODMAN,
            args: vec!["rm".to_string(), self.config.container.name.clone()],
        }
    }

    /// The full run argument list: image, name, cache volume, publish rule,
    /// hardening, log cap, restart policy, and the two deployment environment values.
    pub fn run_command(&self) -> PlatformCommand {
        let container = &self.config.container;
        PlatformCommand {
            program: PODMAN,
            args: vec![
                "run".to_string(),
                "-d".to_string(),
                "--name".to_string(),
                container.name.clone(),
                "--restart".to_string(),
                "unless-stopped".to_string(),
                "--read-only".to_string(),
                "--tmpfs".to_string(),
                "/tmp".to_string(),
                "--cap-drop=ALL".to_string(),
                "--security-opt".to_string(),
                "no-new-privileges".to_string(),
                // The host default is journald, where max-size has no meaning.
                // k8s-file is Podman's file driver and honours the cap.
                "--log-driver=k8s-file".to_string(),
                "--log-opt".to_string(),
                "max-size=10m".to_string(),
                "-p".to_string(),
                self.publish.clone(),
                "-v".to_string(),
                format!("{}:/var/cache/ntfy", container.cache_volume),
                "-e".to_string(),
                self.env[0].clone(),
                "-e".to_string(),
                self.env[1].clone(),
                container.image.clone(),
            ],
        }
    }

    pub fn planned_lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("reuse check: {}", self.inspect_command()),
            format!("image check: {}", self.image_inspect_command()),
            format!("+ {}", self.stop_command()),
            format!("+ {}", self.remove_command()),
            format!("+ {}", self.run_command()),
            format!("health: {}", self.health_url()),
        ];
        if self.platform == Platform::Windows {
            for entry in &self.portproxy {
                lines.push(format!("portproxy: ensure {entry}"));
            }
            lines.push("portproxy: remove other managed listeners".to_string());
        }
        lines
    }
}

pub struct Deployer {
    runner: Arc<dyn crate::runner::CommandRunner>,
    probe: Arc<dyn HealthProbe>,
    platform: Platform,
    retry: HealthRetry,
}

impl Deployer {
    pub fn new(
        runner: Arc<dyn crate::runner::CommandRunner>,
        probe: Arc<dyn HealthProbe>,
        platform: Platform,
        retry: HealthRetry,
    ) -> Self {
        Self {
            runner,
            probe,
            platform,
            retry,
        }
    }

    pub fn apply(&self, config: &DeployConfig, dry_run: bool) -> Result<Report, DeployError> {
        let plan = Plan::new(config, self.platform)?;
        if dry_run {
            return Ok(Report {
                summary: plan.summary(),
                health: None,
                state: ApplyState::Planned,
                commands: plan.planned_lines(),
                warnings: Vec::new(),
                dry_run: true,
            });
        }

        let info = self.inspect(&plan)?;
        let local_image = match &info {
            Some(_) => self.local_image_id(&plan)?,
            None => None,
        };
        let mut warnings = Vec::new();
        if let Some(info) = &info {
            if info.image_id.is_empty() || local_image.is_none() {
                warnings.push(
                    "image-id unknown: the container and tag image ids cannot be compared; recreating"
                        .to_string(),
                );
            }
        }
        let reusable = info
            .as_ref()
            .is_some_and(|info| info.matches(&plan, local_image.as_deref()));
        let healthy = reusable && self.probe.get(config.port).is_ok_and(|code| code == 200);
        let mut commands = Vec::new();
        let state = if healthy {
            ApplyState::Reused
        } else {
            let stop = plan.stop_command();
            let _ = self.runner.run(stop.program, &stop.args)?;
            commands.push(stop.to_string());
            let remove = plan.remove_command();
            let _ = self.runner.run(remove.program, &remove.args)?;
            commands.push(remove.to_string());
            let run = plan.run_command();
            self.execute(&run)?;
            commands.push(run.to_string());
            ApplyState::Recreated
        };

        // The loopback bridge carries the relay and the TLS terminator in every
        // mode, so it must exist before the loopback health probe.
        if self.platform == Platform::Windows {
            let current = self.show_portproxy()?;
            let loopback = &plan.portproxy[..1];
            let managed = [LOOPBACK_ADDRESS.to_string()];
            for command in portproxy_commands(&current, loopback, &managed, plan.config.port) {
                self.execute(&command)?;
                commands.push(command.to_string());
            }
        }

        let health = self.wait_healthy(plan.config.port)?;

        if self.platform == Platform::Windows {
            let current = self.show_portproxy()?;
            for command in
                portproxy_commands(&current, &plan.portproxy, &plan.managed, plan.config.port)
            {
                self.execute(&command)?;
                commands.push(command.to_string());
            }
        }

        Ok(Report {
            summary: plan.summary(),
            health: Some(health),
            state,
            commands,
            warnings,
            dry_run: false,
        })
    }

    fn inspect(&self, plan: &Plan<'_>) -> Result<Option<InspectInfo>, DeployError> {
        let command = plan.inspect_command();
        let output = self.runner.run(command.program, &command.args)?;
        if output.status != 0 {
            return Ok(None);
        }
        Ok(parse_inspect(&output.stdout))
    }

    fn local_image_id(&self, plan: &Plan<'_>) -> Result<Option<String>, DeployError> {
        let command = plan.image_inspect_command();
        let output = self.runner.run(command.program, &command.args)?;
        if output.status != 0 {
            return Ok(None);
        }
        let id = output.stdout.trim();
        Ok((!id.is_empty()).then(|| id.to_string()))
    }

    fn show_portproxy(&self) -> Result<Vec<PortproxyEntry>, DeployError> {
        let command = netsh_show();
        let output = self.runner.run(command.program, &command.args)?;
        if output.status != 0 {
            return Err(DeployError::platform(
                "netsh",
                format!(
                    "portproxy show failed with exit {}: {}",
                    output.status,
                    clip(&output.stderr, 200)
                ),
            ));
        }
        Ok(parse_portproxy_show(&output.stdout))
    }

    fn execute(&self, command: &PlatformCommand) -> Result<(), DeployError> {
        let output = self.runner.run(command.program, &command.args)?;
        if output.status != 0 {
            return Err(DeployError::platform(
                command.program,
                format!(
                    "`{command}` failed with exit {}: {}",
                    output.status,
                    clip(&output.stderr, 200)
                ),
            ));
        }
        Ok(())
    }

    fn wait_healthy(&self, port: u16) -> Result<u16, DeployError> {
        let attempts = self.retry.attempts.max(1);
        let mut detail = String::from("no attempt");
        for attempt in 0..attempts {
            match self.probe.get(port) {
                Ok(200) => return Ok(200),
                Ok(code) => detail = format!("HTTP {code}"),
                Err(error) => detail = error,
            }
            if attempt + 1 < attempts && !self.retry.interval.is_zero() {
                std::thread::sleep(self.retry.interval);
            }
        }
        Err(DeployError::Unhealthy { port, detail })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyState {
    Planned,
    Reused,
    Recreated,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub summary: PlanSummary,
    pub health: Option<u16>,
    pub state: ApplyState,
    pub commands: Vec<String>,
    /// Conditions that changed the decision without failing the run.
    pub warnings: Vec<String>,
    pub dry_run: bool,
}

pub(crate) struct InspectInfo {
    pub running: bool,
    pub image: String,
    pub image_id: String,
    pub env: Vec<String>,
    pub port_bindings: Vec<(String, String)>,
    pub restart: String,
    pub readonly: bool,
    pub cap_drop: Vec<String>,
    pub security_opt: Vec<String>,
    pub tmpfs: Vec<String>,
    pub log_driver: String,
}

impl InspectInfo {
    fn matches(&self, plan: &Plan<'_>, local_image: Option<&str>) -> bool {
        self.running
            && image_matches(&self.image, &plan.config.container.image)
            && image_content_matches(&self.image_id, local_image)
            && self.env.iter().any(|value| value == &plan.env[0])
            && self.env.iter().any(|value| value == &plan.env[1])
            && self.port_bindings.iter().any(|(host, port)| {
                plan.host_allowed(host) && port == &plan.config.port.to_string()
            })
            && self.restart == "unless-stopped"
            && self.readonly
            && self
                .cap_drop
                .iter()
                .any(|value| value.eq_ignore_ascii_case("ALL"))
            && self
                .security_opt
                .iter()
                .any(|value| value.starts_with("no-new-privileges"))
            && self.tmpfs.iter().any(|value| value == "/tmp")
            && (self.log_driver == "k8s-file" || self.log_driver == "json-file")
    }
}

fn image_matches(actual: &str, wanted: &str) -> bool {
    if actual == wanted {
        return true;
    }
    let last = wanted.rsplit('/').next().unwrap_or(wanted);
    !last.contains(':') && actual == format!("{wanted}:latest")
}

/// Reuse needs the container to run the image the tag points at now. An id the
/// host cannot resolve cannot prove a match, so the container is recreated.
fn image_content_matches(container_id: &str, local_id: Option<&str>) -> bool {
    match local_id {
        Some(local) => same_image_id(container_id, local),
        None => false,
    }
}

fn same_image_id(actual: &str, wanted: &str) -> bool {
    let a = normalize_id(actual);
    let b = normalize_id(wanted);
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let short = a.len().min(b.len());
    short >= 12 && (a.starts_with(&b) || b.starts_with(&a))
}

fn normalize_id(id: &str) -> String {
    id.trim().trim_start_matches("sha256:").to_ascii_lowercase()
}

pub(crate) fn parse_inspect(text: &str) -> Option<InspectInfo> {
    let value: Value = serde_json::from_str(text).ok()?;
    let object = if let Some(array) = value.as_array() {
        array.first()?.as_object()?
    } else {
        value.as_object()?
    };
    let state = object.get("State")?;
    let config = object.get("Config");
    let host = object.get("HostConfig");
    let image = object
        .get("ImageName")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| config.and_then(|c| c.get("Image")).and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let image_id = object
        .get("Image")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let env = config
        .and_then(|c| c.get("Env"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let port_bindings = host
        .and_then(|h| h.get("PortBindings"))
        .and_then(|p| p.get(INSPECT_PORT_KEY))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|entry| {
                    (
                        entry
                            .get("HostIp")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        entry
                            .get("HostPort")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let restart = host
        .and_then(|h| h.get("RestartPolicy"))
        .and_then(|r| r.get("Name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let readonly = host
        .and_then(|h| h.get("ReadonlyRootfs"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let cap_drop = host
        .and_then(|h| h.get("CapDrop"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let security_opt = host
        .and_then(|h| h.get("SecurityOpt"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let tmpfs = host
        .and_then(|h| h.get("Tmpfs"))
        .and_then(Value::as_object)
        .map(|values| values.keys().cloned().collect())
        .unwrap_or_default();
    let log_driver = host
        .and_then(|h| h.get("LogConfig"))
        .and_then(|config| config.get("Type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(InspectInfo {
        running: state
            .get("Running")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        image,
        image_id,
        env,
        port_bindings,
        restart,
        readonly,
        cap_drop,
        security_opt,
        tmpfs,
        log_driver,
    })
}

fn clip(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= limit {
        return trimmed.to_string();
    }
    let mut cut = limit;
    while cut > 0 && !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &trimmed[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeployConfig;
    use crate::test_support::{FakeProbe, FakeRunner};

    const SHOW_LOOPBACK: &str = "
Listen on ipv4:             Connect to ipv6:

Address         Port        Address         Port
--------------- ----------  --------------- ----------
127.0.0.1       8810        ::1             8810
";

    const SHOW_BOTH: &str = "
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

    fn inspect(config: &DeployConfig, running: bool, extra_env: bool, image_id: &str) -> String {
        inspect_at(config, running, extra_env, image_id, "0.0.0.0", true)
    }

    fn inspect_at(
        config: &DeployConfig,
        running: bool,
        extra_env: bool,
        image_id: &str,
        host: &str,
        hardening: bool,
    ) -> String {
        let mut env = vec!["container=podman".to_string()];
        if extra_env {
            env.push(format!("NTFY_BASE_URL={}", config.base_url));
            env.push(format!("NTFY_BEHIND_PROXY={}", config.behind_proxy));
        }
        let mut host_config = serde_json::json!({
            "PortBindings": {
                "8080/tcp": [{ "HostIp": host, "HostPort": config.port.to_string() }]
            },
            "RestartPolicy": { "Name": "unless-stopped" },
            "ReadonlyRootfs": true
        });
        if hardening {
            host_config["CapDrop"] = serde_json::json!(["ALL"]);
            host_config["SecurityOpt"] = serde_json::json!(["no-new-privileges"]);
            host_config["Tmpfs"] = serde_json::json!({ "/tmp": "" });
            host_config["LogConfig"] = serde_json::json!({ "Type": "k8s-file", "Config": {} });
        }
        let mut entry = serde_json::json!({
            "ImageName": config.container.image,
            "State": { "Running": running },
            "Config": { "Env": env, "Image": config.container.image },
            "HostConfig": host_config
        });
        if !image_id.is_empty() {
            entry["Image"] = serde_json::Value::String(image_id.to_string());
        }
        serde_json::json!([entry]).to_string()
    }

    fn deployer(runner: Arc<FakeRunner>, probe: Arc<FakeProbe>, platform: Platform) -> Deployer {
        Deployer::new(
            runner,
            probe,
            platform,
            HealthRetry {
                attempts: 3,
                interval: Duration::ZERO,
            },
        )
    }

    #[test]
    fn dry_run_prints_the_plan_and_runs_nothing() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let runner = Arc::new(FakeRunner::new());
        let probe = Arc::new(FakeProbe::always(200));
        let deployer = deployer(runner.clone(), probe, Platform::Windows);
        let report = deployer.apply(&config, true).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.state, ApplyState::Planned);
        assert!(report
            .commands
            .iter()
            .any(|line| line.contains("podman run -d --name ntfy")));
        assert!(report
            .commands
            .iter()
            .any(|line| line.contains("NTFY_BASE_URL=https://ntfy.example.ts.net")));
        assert_eq!(runner.count(), 0);
    }

    #[test]
    fn a_matching_healthy_container_is_reused() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let id = "6c7427154d80501f7d6e4a467ed1ca369af28e4a1c2c2603640e0a71f83fea33";
        let runner = Arc::new(FakeRunner::with_inspect(&inspect(&config, true, true, id)));
        runner.set_image_id(id);
        runner.set_show(SHOW_LOOPBACK);
        let probe = Arc::new(FakeProbe::always(200));
        let deployer = deployer(runner.clone(), probe, Platform::Windows);
        let report = deployer.apply(&config, false).unwrap();
        assert_eq!(report.state, ApplyState::Reused);
        let record = runner.calls();
        assert!(!record
            .iter()
            .any(|(_, args)| args.first().map(String::as_str) == Some("run")));
        assert!(!record
            .iter()
            .any(|(_, args)| args.first().map(String::as_str) == Some("stop")));
    }

    #[test]
    fn an_unknown_image_id_is_recreated_with_a_warning() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let runner = Arc::new(FakeRunner::with_inspect(&inspect(&config, true, true, "")));
        runner.set_show(SHOW_LOOPBACK);
        let probe = Arc::new(FakeProbe::always(200));
        let deployer = deployer(runner, probe, Platform::Windows);
        let report = deployer.apply(&config, false).unwrap();
        assert_eq!(report.state, ApplyState::Recreated);
        assert!(
            report
                .warnings
                .iter()
                .any(|line| line.contains("image-id unknown")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn a_linux_proxy_container_on_a_wildcard_is_recreated() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let id = "6c7427154d80501f7d6e4a467ed1ca369af28e4a1c2c2603640e0a71f83fea33";
        let runner = Arc::new(FakeRunner::with_inspect(&inspect_at(
            &config, true, true, id, "0.0.0.0", true,
        )));
        runner.set_image_id(id);
        let probe = Arc::new(FakeProbe::always(200));
        let deployer = deployer(runner.clone(), probe, Platform::Linux);
        let report = deployer.apply(&config, false).unwrap();
        assert_eq!(report.state, ApplyState::Recreated);
        assert!(runner
            .calls()
            .iter()
            .any(|(_, args)| args.first().map(String::as_str) == Some("run")));
    }

    #[test]
    fn a_linux_proxy_container_on_loopback_is_reused() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let id = "6c7427154d80501f7d6e4a467ed1ca369af28e4a1c2c2603640e0a71f83fea33";
        let runner = Arc::new(FakeRunner::with_inspect(&inspect_at(
            &config,
            true,
            true,
            id,
            "127.0.0.1",
            true,
        )));
        runner.set_image_id(id);
        let probe = Arc::new(FakeProbe::always(200));
        let deployer = deployer(runner, probe, Platform::Linux);
        let report = deployer.apply(&config, false).unwrap();
        assert_eq!(report.state, ApplyState::Reused);
    }

    #[test]
    fn a_container_without_hardening_is_recreated() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let id = "6c7427154d80501f7d6e4a467ed1ca369af28e4a1c2c2603640e0a71f83fea33";
        let runner = Arc::new(FakeRunner::with_inspect(&inspect_at(
            &config,
            true,
            true,
            id,
            "127.0.0.1",
            false,
        )));
        runner.set_image_id(id);
        let probe = Arc::new(FakeProbe::always(200));
        let deployer = deployer(runner, probe, Platform::Linux);
        let report = deployer.apply(&config, false).unwrap();
        assert_eq!(report.state, ApplyState::Recreated);
    }

    #[test]
    fn a_container_with_journald_logs_is_recreated() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let id = "6c7427154d80501f7d6e4a467ed1ca369af28e4a1c2c2603640e0a71f83fea33";
        let journald = inspect_at(&config, true, true, id, "127.0.0.1", true)
            .replace("\"k8s-file\"", "\"journald\"");
        let runner = Arc::new(FakeRunner::with_inspect(&journald));
        runner.set_image_id(id);
        let probe = Arc::new(FakeProbe::always(200));
        let deployer = deployer(runner, probe, Platform::Linux);
        let report = deployer.apply(&config, false).unwrap();
        assert_eq!(report.state, ApplyState::Recreated);
    }

    #[test]
    fn a_mismatched_container_is_recreated() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let runner = Arc::new(FakeRunner::with_inspect(&inspect(&config, true, false, "")));
        runner.set_show(SHOW_LOOPBACK);
        let probe = Arc::new(FakeProbe::always(200));
        let deployer = deployer(runner.clone(), probe, Platform::Windows);
        let report = deployer.apply(&config, false).unwrap();
        assert_eq!(report.state, ApplyState::Recreated);
        let calls = runner.calls();
        let run = calls
            .iter()
            .find(|(_, args)| args.first().map(String::as_str) == Some("run"))
            .expect("run call");
        assert!(run
            .1
            .contains(&"NTFY_BASE_URL=https://ntfy.example.ts.net".to_string()));
        assert!(run.1.contains(&"NTFY_BEHIND_PROXY=true".to_string()));
        assert!(run.1.contains(&"8810:8080".to_string()));
        assert!(run.1.contains(&"ntfy-cache:/var/cache/ntfy".to_string()));
        assert!(run.1.contains(&"--log-driver=k8s-file".to_string()));
        assert!(run.1.contains(&"max-size=10m".to_string()));
    }

    #[test]
    fn an_unhealthy_matching_container_is_recreated() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let runner = Arc::new(FakeRunner::with_inspect(&inspect(&config, true, true, "")));
        runner.set_show(SHOW_LOOPBACK);
        let probe = Arc::new(FakeProbe::sequence(vec![Ok(500), Ok(200)]));
        let deployer = deployer(runner, probe, Platform::Windows);
        let report = deployer.apply(&config, false).unwrap();
        assert_eq!(report.state, ApplyState::Recreated);
        assert_eq!(report.health, Some(200));
    }

    #[test]
    fn vpn_adds_the_portproxy_after_health() {
        let config = config("vpn", "http://100.64.0.10:8810");
        let runner = Arc::new(FakeRunner::new());
        runner.set_show(SHOW_LOOPBACK);
        let probe = Arc::new(FakeProbe::always(200));
        let deployer = deployer(runner, probe, Platform::Windows);
        let report = deployer.apply(&config, false).unwrap();
        assert!(report
            .commands
            .iter()
            .any(|line| line.contains("add v4tov6") && line.contains("listenaddress=100.64.0.10")));
    }

    #[test]
    fn proxy_removes_the_vpn_portproxy() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let runner = Arc::new(FakeRunner::with_inspect(&inspect(&config, true, true, "")));
        runner.set_show(SHOW_BOTH);
        let probe = Arc::new(FakeProbe::always(200));
        let deployer = deployer(runner, probe, Platform::Windows);
        let report = deployer.apply(&config, false).unwrap();
        assert!(report.commands.iter().any(
            |line| line.contains("delete v4tov6") && line.contains("listenaddress=100.64.0.10")
        ));
    }

    #[test]
    fn a_failed_run_maps_to_exit_4() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let runner = Arc::new(FakeRunner::new());
        runner.fail_on("run", 125);
        let probe = Arc::new(FakeProbe::always(200));
        let deployer = deployer(runner, probe, Platform::Windows);
        let error = deployer.apply(&config, false).unwrap_err();
        assert_eq!(error.exit_code(), 4);
    }

    #[test]
    fn an_unhealthy_apply_maps_to_exit_3() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let deployer = deployer(
            Arc::new(FakeRunner::new()),
            Arc::new(FakeProbe::always(503)),
            Platform::Windows,
        );
        let error = deployer.apply(&config, false).unwrap_err();
        assert_eq!(error.exit_code(), 3);
    }

    #[test]
    fn invalid_config_maps_to_exit_2() {
        let error = DeployError::from(DeployConfig::parse_text("{}").unwrap_err());
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn linux_apply_uses_all_interfaces_outside_proxy() {
        let config = config("vpn", "http://100.64.0.10:8810");
        let deployer = deployer(
            Arc::new(FakeRunner::new()),
            Arc::new(FakeProbe::always(200)),
            Platform::Linux,
        );
        let report = deployer.apply(&config, false).unwrap();
        assert_eq!(report.summary.publish, "8810:8080");
        assert!(report
            .commands
            .iter()
            .any(|line| line.contains("8810:8080")));
    }

    #[test]
    fn a_rebuilt_image_with_the_same_tag_is_recreated() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let runner = Arc::new(FakeRunner::with_inspect(&inspect(
            &config,
            true,
            true,
            "6c7427154d80501f7d6e4a467ed1ca369af28e4a1c2c2603640e0a71f83fea33",
        )));
        runner.set_image_id("908dd86ed68d562556cdf1af6a49f9fe716570aa683a372f30c1a40bfa410825");
        runner.set_show(SHOW_LOOPBACK);
        let probe = Arc::new(FakeProbe::always(200));
        let deployer = deployer(runner.clone(), probe, Platform::Windows);
        let report = deployer.apply(&config, false).unwrap();
        assert_eq!(report.state, ApplyState::Recreated);
        assert!(runner
            .calls()
            .iter()
            .any(|(_, args)| args.first().map(String::as_str) == Some("run")));
    }

    #[test]
    fn a_container_on_the_tagged_image_is_reused() {
        let config = config("proxy", "https://ntfy.example.ts.net");
        let runner = Arc::new(FakeRunner::with_inspect(&inspect(
            &config,
            true,
            true,
            "sha256:6C7427154D80501F7D6E4A467ED1CA369AF28E4A1C2C2603640E0A71F83FEA33",
        )));
        runner.set_image_id("6c7427154d80501f7d6e4a467ed1ca369af28e4a1c2c2603640e0a71f83fea33");
        runner.set_show(SHOW_LOOPBACK);
        let probe = Arc::new(FakeProbe::always(200));
        let deployer = deployer(runner.clone(), probe, Platform::Windows);
        let report = deployer.apply(&config, false).unwrap();
        assert_eq!(report.state, ApplyState::Reused);
    }

    #[test]
    fn image_ids_compare_through_the_sha256_prefix_and_short_forms() {
        assert!(same_image_id("sha256:ABC123456789", "abc123456789"));
        assert!(same_image_id("6c7427154d80", "6c7427154d80501f"));
        assert!(same_image_id("6c7427154d80501f", "6c7427154d80"));
        assert!(!same_image_id("6c7427154d80501f", "6c7427154d80501e"));
        assert!(!same_image_id("", "abc123456789"));
        assert!(same_image_id("abc", "abc"));
    }

    #[test]
    fn inspect_parsing_accepts_empty_image_name() {
        let text = r#"[{"State":{"Running":true},"Config":{"Image":"img:1"},"HostConfig":{}}]"#;
        let info = parse_inspect(text).unwrap();
        assert_eq!(info.image, "img:1");
        assert_eq!(info.image_id, "");
        assert!(info.running);
        assert!(!info.readonly);
    }

    #[test]
    fn inspect_parsing_reads_the_image_id() {
        let text = r#"[{"State":{"Running":true},"Image":"6c7427154d80","Config":{"Image":"img:1"},"HostConfig":{}}]"#;
        let info = parse_inspect(text).unwrap();
        assert_eq!(info.image_id, "6c7427154d80");
    }

    #[test]
    fn image_comparison_accepts_the_implicit_latest_tag() {
        assert!(image_matches("img:latest", "img"));
        assert!(image_matches("img:1", "img:1"));
        assert!(!image_matches("img:2", "img:1"));
    }

    #[test]
    fn status_line_parsing() {
        assert_eq!(parse_status_line("HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(parse_status_line("nope"), None);
    }
}
