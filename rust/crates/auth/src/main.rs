//! `ntfy-auth apply|status`.
//!
//! Exit codes: 0 ok, 2 invalid config, 3 incomplete, 4 platform command failed.

use std::path::{Path, PathBuf};

use ntfy_relay_auth::auth::{Change, Report, StatusReport, TokenState};
use ntfy_relay_auth::{AuthConfig, AuthEngine, SystemRunner};

const USAGE: &str = "usage: ntfy-auth apply [--config <path>] [--dry-run]\n       ntfy-auth status [--config <path>]";
const CONFIG_FILE: &str = "ntfy-auth.json";

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
    if command != "apply" && command != "status" {
        eprintln!("{USAGE}");
        return 2;
    }
    let mut config_path: Option<PathBuf> = None;
    let mut dry_run = false;
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
            "--dry-run" if command == "apply" => dry_run = true,
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!("{USAGE}");
                return 2;
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
    let config = match AuthConfig::load(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("FAIL {error}");
            return 2;
        }
    };
    let root = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let token_path = config.token_path(&root);
    let engine = AuthEngine::new(SystemRunner);
    if command == "apply" {
        match engine.apply(&config, &token_path, dry_run) {
            Ok(report) => {
                print_report(&config, &report);
                0
            }
            Err(error) => {
                eprintln!("FAIL {error}");
                error.exit_code()
            }
        }
    } else {
        match engine.status(&config, &token_path) {
            Ok(report) => {
                let code = if report.ok() { 0 } else { 3 };
                print_status(&config, &report);
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

fn change_label(change: Change, dry_run: bool) -> &'static str {
    match (change, dry_run) {
        (Change::Created, false) => "created",
        (Change::Created, true) => "would create",
        (Change::Kept, _) => "kept",
    }
}

fn print_report(config: &AuthConfig, report: &Report) {
    println!("container: {}", config.container);
    println!(
        "user: {} ({})",
        config.user,
        change_label(report.user, report.dry_run)
    );
    println!(
        "access: {} {} ({})",
        config.user,
        config.topic,
        change_label(report.access, report.dry_run)
    );
    println!(
        "token: {} ({})",
        change_label(report.token, report.dry_run),
        config.token_file
    );
    if report.dry_run {
        println!("dry-run: no changes");
        for line in &report.plan {
            println!("plan: {line}");
        }
    } else {
        println!("result: applied");
    }
}

fn print_status(config: &AuthConfig, report: &StatusReport) {
    println!("container: {}", config.container);
    println!(
        "user: {} ({})",
        config.user,
        if report.user { "present" } else { "missing" }
    );
    println!(
        "access: {} {} ({})",
        config.user,
        config.topic,
        if report.access { "present" } else { "missing" }
    );
    let token = match report.token {
        TokenState::Listed => "listed",
        TokenState::NotListed => "file token not listed",
        TokenState::Missing => "file missing",
    };
    println!("token: {token} ({})", config.token_file);
    println!("result: {}", if report.ok() { "ok" } else { "incomplete" });
}
