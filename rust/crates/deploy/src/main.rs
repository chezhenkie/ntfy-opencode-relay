//! `ntfy-deploy apply|status`.
//!
//! Exit codes: 0 ok, 2 invalid config, 3 unhealthy, 4 platform command failed.

use std::path::PathBuf;
use std::sync::Arc;

use ntfy_relay_deploy::config::DeployConfig;
use ntfy_relay_deploy::deploy::{ApplyState, Deployer, HealthRetry, Report, TcpHealth};
use ntfy_relay_deploy::mode::{self, Choice};
use ntfy_relay_deploy::platform::Platform;
use ntfy_relay_deploy::status::{self, StatusReport};
use ntfy_relay_deploy::SystemRunner;

const USAGE: &str = "usage: ntfy-deploy apply [--config <path>] [--dry-run]\n       ntfy-deploy status [--config <path>]\n       ntfy-deploy mode [tailscale|nord|lan] [--config <path>] [--dry-run]";
const CONFIG_FILE: &str = "ntfy-deploy.json";

fn main() {
    std::process::exit(dispatch());
}

fn dispatch() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        eprintln!("{USAGE}");
        return 2;
    };
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        println!("{USAGE}");
        return 0;
    }
    if command != "apply" && command != "status" && command != "mode" {
        eprintln!("{USAGE}");
        return 2;
    }
    let mut config_path: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut target: Option<String> = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => {
                index += 1;
                match args.get(index) {
                    Some(value) if !value.is_empty() => {
                        config_path = Some(PathBuf::from(value));
                    }
                    _ => {
                        eprintln!("--config needs a path");
                        return 2;
                    }
                }
            }
            "--dry-run" if command == "apply" || command == "mode" => dry_run = true,
            other => {
                let positional = command == "mode" && target.is_none() && !other.starts_with('-');
                if positional {
                    target = Some(other.to_string());
                } else {
                    eprintln!("unknown argument: {other}");
                    eprintln!("{USAGE}");
                    return 2;
                }
            }
        }
        index += 1;
    }
    let path = match config_path {
        Some(path) => path,
        None => match default_config_path() {
            Ok(path) => path,
            Err(message) => {
                eprintln!("FAIL {message}");
                return 2;
            }
        },
    };
    let config = match DeployConfig::load(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("FAIL {error}");
            return 2;
        }
    };
    let deployer = Deployer::new(
        Arc::new(SystemRunner),
        Arc::new(TcpHealth::new()),
        Platform::current(),
        HealthRetry::default(),
    );
    if command == "apply" {
        match deployer.apply(&config, dry_run) {
            Ok(report) => {
                print_report(&report);
                0
            }
            Err(error) => {
                eprintln!("FAIL {error}");
                error.exit_code()
            }
        }
    } else if command == "mode" {
        run_mode(&deployer, &config, &path, dry_run, target.as_deref())
    } else {
        match status::gather(
            &SystemRunner,
            &TcpHealth::new(),
            Platform::current(),
            &config,
        ) {
            Ok(report) => {
                let code = if report.ok { 0 } else { 3 };
                print_status(&report);
                code
            }
            Err(error) => {
                eprintln!("FAIL {error}");
                error.exit_code()
            }
        }
    }
}

/// `NTFY_RELAY_HOME` for tests and staging, else the directory that holds the
/// running binary. No home-directory fallback.
fn default_config_path() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("NTFY_RELAY_HOME") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value).join(CONFIG_FILE));
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .map(|root| root.join(CONFIG_FILE))
        .ok_or_else(|| "cannot determine the install root".to_string())
}

/// `mode`: detect the paths, let the operator pick one, write the config, and
/// apply it. The relay is never touched.
fn run_mode(
    deployer: &Deployer,
    current: &DeployConfig,
    path: &std::path::Path,
    dry_run: bool,
    target: Option<&str>,
) -> i32 {
    let platform = Platform::current();
    let choices = match mode::detect(&SystemRunner, platform, current.port, current) {
        Ok(choices) => choices,
        Err(error) => {
            eprintln!("FAIL {error}");
            return error.exit_code();
        }
    };
    if choices.is_empty() {
        eprintln!(
            "FAIL no network path detected: start Tailscale, connect Nord meshnet, or check the default interface"
        );
        return 2;
    }
    let choice = match target {
        Some(id) => match mode::find_choice(&choices, id) {
            Some(choice) => choice.clone(),
            None => {
                let names: Vec<&str> = choices.iter().map(|choice| choice.id).collect();
                eprintln!("FAIL {id} is not available; detected: {}", names.join(", "));
                return 2;
            }
        },
        None => match select(&choices) {
            Ok(Some(choice)) => choice,
            Ok(None) => {
                println!("cancelled");
                return 0;
            }
            Err(message) => {
                eprintln!("FAIL {message}");
                return 2;
            }
        },
    };

    let next = mode::config_from_choice(current, &choice);
    if dry_run {
        println!("config: {}", path.display());
        println!("{}", next.to_json().unwrap_or_default());
        match deployer.apply(&next, true) {
            Ok(report) => {
                print_report(&report);
                0
            }
            Err(error) => {
                eprintln!("FAIL {error}");
                error.exit_code()
            }
        }
    } else {
        if let Err(error) = mode::write_config(path, &next) {
            eprintln!("FAIL {error}");
            return 2;
        }
        println!("config written: {}", path.display());
        match deployer.apply(&next, false) {
            Ok(report) => {
                print_report(&report);
                println!("phone server URL: {}", choice.base_url);
                println!("set this URL in the app if it changed; the token stays the same.");
                0
            }
            Err(error) => {
                eprintln!("FAIL {error}");
                if error.to_string().to_ascii_lowercase().contains("netsh") {
                    eprintln!("note: portproxy changes need an elevated shell");
                }
                error.exit_code()
            }
        }
    }
}

fn select(choices: &[Choice]) -> Result<Option<Choice>, String> {
    println!("available network paths:");
    for (index, choice) in choices.iter().enumerate() {
        println!("{}", choice.line(index + 1));
    }
    println!();
    println!("select 1-{} or press Enter to cancel:", choices.len());
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return Ok(None);
    }
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    match line.parse::<usize>() {
        Ok(number) if (1..=choices.len()).contains(&number) => {
            Ok(Some(choices[number - 1].clone()))
        }
        _ => Err(format!("invalid selection: {line}")),
    }
}

fn print_report(report: &Report) {
    let summary = &report.summary;
    println!("mode: {}", summary.mode.as_str());
    println!("base url: {}", summary.base_url);
    println!("platform: {}", summary.platform.as_str());
    println!("container: {} ({})", summary.container, summary.image);
    println!("publish: {}", summary.publish);
    println!("env: {} {}", summary.env[0], summary.env[1]);
    if report.dry_run {
        println!("dry-run: no changes");
        for line in &report.commands {
            println!("{line}");
        }
        return;
    }
    match report.state {
        ApplyState::Reused => println!("container state: reused"),
        ApplyState::Recreated => println!("container state: recreated"),
        ApplyState::Planned => println!("container state: planned"),
    }
    if let Some(code) = report.health {
        println!("health: {code}");
    }
    for entry in &summary.portproxy {
        println!("portproxy: {entry}");
    }
    for command in &report.commands {
        println!("changed: {command}");
    }
    println!("result: applied");
}

fn print_status(report: &StatusReport) {
    let summary = &report.summary;
    println!("mode: {}", summary.mode.as_str());
    println!("base url: {}", summary.base_url);
    println!("platform: {}", summary.platform.as_str());
    match report.container.image() {
        Some(image) => println!("container: {} ({image})", report.container.label()),
        None => println!("container: {}", report.container.label()),
    }
    println!("publish: {}", summary.publish);
    match report.health {
        Some(code) => println!("health: {code}"),
        None => println!("health: unreachable"),
    }
    if report.summary.platform == Platform::Windows {
        for line in &report.listeners {
            println!("portproxy: {line}");
        }
    } else {
        for line in &report.listeners {
            println!("listener: {line}");
        }
    }
    println!("status: {}", if report.ok { "ok" } else { "unhealthy" });
}
