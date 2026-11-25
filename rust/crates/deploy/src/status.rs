//! `ntfy-deploy status`: config, container, health, and the active listener.
//!
//! Exit 0 when the container runs, loopback health is 200, and the rules the
//! mode requires are present; exit 3 otherwise. A platform command that cannot
//! run is exit 4.

use crate::config::DeployConfig;
use crate::deploy::{parse_inspect, DeployError, HealthProbe, Plan, PlanSummary};
use crate::platform::{netsh_show, parse_portproxy_show, Platform, PortproxyEntry};
use crate::runner::CommandRunner;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerState {
    Running(String),
    Stopped(String),
    Absent,
}

impl ContainerState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running(_) => "running",
            Self::Stopped(_) => "stopped",
            Self::Absent => "absent",
        }
    }

    pub fn image(&self) -> Option<&str> {
        match self {
            Self::Running(image) | Self::Stopped(image) => Some(image),
            Self::Absent => None,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running(_))
    }
}

#[derive(Debug, Clone)]
pub struct StatusReport {
    pub summary: PlanSummary,
    pub container: ContainerState,
    pub health: Option<u16>,
    pub listeners: Vec<String>,
    pub active_portproxy: Vec<PortproxyEntry>,
    pub ok: bool,
}

pub fn gather(
    runner: &dyn CommandRunner,
    probe: &dyn HealthProbe,
    platform: Platform,
    config: &DeployConfig,
) -> Result<StatusReport, DeployError> {
    let plan = Plan::new(config, platform)?;
    let args = vec!["inspect".to_string(), config.container.name.clone()];
    let output = runner.run("podman", &args)?;
    let info = if output.status == 0 {
        parse_inspect(&output.stdout)
    } else {
        None
    };
    let container = match &info {
        Some(info) if info.running => ContainerState::Running(info.image.clone()),
        Some(info) => ContainerState::Stopped(info.image.clone()),
        None => ContainerState::Absent,
    };
    let health = probe.get(config.port).ok();

    let mut listeners = Vec::new();
    let mut active_portproxy = Vec::new();
    let mut rules_ok = true;
    match platform {
        Platform::Windows => {
            let command = netsh_show();
            let output = runner.run(command.program, &command.args)?;
            if output.status != 0 {
                return Err(DeployError::platform(
                    "netsh",
                    format!(
                        "portproxy show failed with exit {}: {}",
                        output.status,
                        output.stderr.trim()
                    ),
                ));
            }
            active_portproxy = parse_portproxy_show(&output.stdout)
                .into_iter()
                .filter(|entry| entry.listen_port == config.port)
                .collect();
            listeners = active_portproxy.iter().map(ToString::to_string).collect();
            rules_ok = plan
                .portproxy
                .iter()
                .all(|expected| active_portproxy.contains(expected));
        }
        Platform::Linux => {
            if let Some(info) = &info {
                for (host, port) in &info.port_bindings {
                    let address = if host.is_empty() {
                        "all"
                    } else {
                        host.as_str()
                    };
                    listeners.push(format!("{address}:{port}"));
                }
            }
        }
    }

    let ok = container.is_running() && health == Some(200) && rules_ok;
    Ok(StatusReport {
        summary: plan.summary(),
        container,
        health,
        listeners,
        active_portproxy,
        ok,
    })
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
    "image": "localhost/ntfy:server",
    "cacheVolume": "ntfy-cache"
  }},
  "vpn": {{ "ip": "100.64.0.10" }}
}}"#,
            mode == "proxy"
        );
        DeployConfig::parse_text(&data).unwrap()
    }

    fn inspect(running: bool) -> String {
        serde_json::json!([{
            "ImageName": "localhost/ntfy:server",
            "State": { "Running": running },
            "Config": { "Env": [], "Image": "localhost/ntfy:server" },
            "HostConfig": {
                "PortBindings": {
                    "8080/tcp": [{ "HostIp": "0.0.0.0", "HostPort": "8810" }]
                },
                "RestartPolicy": { "Name": "unless-stopped" },
                "ReadonlyRootfs": true
            }
        }])
        .to_string()
    }

    #[test]
    fn proxy_status_is_ok_when_health_and_loopback_rule_match() {
        let runner = FakeRunner::with_inspect(&inspect(true));
        runner.set_show(SHOW_LOOPBACK);
        let report = gather(
            &runner,
            &FakeProbe::always(200),
            Platform::Windows,
            &config("proxy", "https://ntfy.example.ts.net"),
        )
        .unwrap();
        assert!(report.ok);
        assert_eq!(report.container.label(), "running");
        assert_eq!(report.health, Some(200));
        assert_eq!(report.listeners.len(), 1);
    }

    #[test]
    fn vpn_status_reports_a_missing_vpn_rule() {
        let runner = FakeRunner::with_inspect(&inspect(true));
        runner.set_show(SHOW_LOOPBACK);
        let report = gather(
            &runner,
            &FakeProbe::always(200),
            Platform::Windows,
            &config("vpn", "http://100.64.0.10:8810"),
        )
        .unwrap();
        assert!(!report.ok);
        assert_eq!(report.active_portproxy.len(), 1);
    }

    #[test]
    fn an_absent_container_is_unhealthy() {
        let runner = FakeRunner::with_show(SHOW_LOOPBACK);
        let report = gather(
            &runner,
            &FakeProbe::always(503),
            Platform::Windows,
            &config("proxy", "https://ntfy.example.ts.net"),
        )
        .unwrap();
        assert!(!report.ok);
        assert_eq!(report.container, ContainerState::Absent);
        assert_eq!(report.health, Some(503));
    }

    #[test]
    fn a_stopped_container_reports_its_image() {
        let runner = FakeRunner::with_inspect(&inspect(false));
        runner.set_show(SHOW_LOOPBACK);
        let report = gather(
            &runner,
            &FakeProbe::always(503),
            Platform::Windows,
            &config("proxy", "https://ntfy.example.ts.net"),
        )
        .unwrap();
        assert!(!report.ok);
        assert_eq!(report.container.label(), "stopped");
        assert_eq!(report.container.image(), Some("localhost/ntfy:server"));
    }

    #[test]
    fn netsh_failure_is_exit_4() {
        let runner = FakeRunner::with_inspect(&inspect(true));
        runner.fail_on("show", 1);
        let error = gather(
            &runner,
            &FakeProbe::always(200),
            Platform::Windows,
            &config("proxy", "https://ntfy.example.ts.net"),
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 4);
    }

    #[test]
    fn linux_status_lists_the_publish_binding() {
        let runner = FakeRunner::with_inspect(&inspect(true));
        let report = gather(
            &runner,
            &FakeProbe::always(200),
            Platform::Linux,
            &config("lan", "http://192.168.1.20:8810"),
        )
        .unwrap();
        assert!(report.ok);
        assert_eq!(report.listeners, vec!["0.0.0.0:8810".to_string()]);
        assert!(report.active_portproxy.is_empty());
    }

    #[test]
    fn status_plan_summary_names_the_mode_and_publish() {
        let runner = FakeRunner::with_inspect(&inspect(true));
        let report = gather(
            &runner,
            &FakeProbe::always(200),
            Platform::Linux,
            &config("proxy", "https://ntfy.example.ts.net"),
        )
        .unwrap();
        assert_eq!(report.summary.mode.as_str(), "proxy");
        assert_eq!(report.summary.publish, "127.0.0.1:8810:8080");
    }
}
