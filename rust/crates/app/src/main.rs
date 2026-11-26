//! `ntfy-relay run|mcp|ensure|status`.
//!
//! One binary for the daemon, the stdio MCP server, and the one-shot checks,
//! so the tool contract and the outbound policy are the same build, as
//! `docs/specsheet/06-rust-crate-split.md` requires.

// A windowless release build on Windows, so a scheduled start shows no console
// window and no hidden-start wrapper is needed. Stdio for `mcp` comes from the
// parent as pipes, so it is unaffected. Debug builds stay console-attached.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod api_client;
mod config_write;
mod control_endpoint;
mod discovery;
mod ensure;
mod lock;
mod logger;
mod mcp;
mod relay;
mod runner;
mod sse;
mod status;
mod status_checks;
mod store;
mod supervisor;
mod util;

const USAGE: &str = "usage: ntfy-relay run|mcp|ensure|status";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Run,
    Mcp,
    Ensure,
    Status,
    Help,
}

/// One subcommand, or `None` for anything else, which prints the usage and
/// exits 2. The arguments are already lossy, so a non-Unicode argument fails
/// here instead of aborting the process.
fn parse_command(args: &[String]) -> Option<Command> {
    match args {
        [command] if command == "run" => Some(Command::Run),
        [command] if command == "mcp" => Some(Command::Mcp),
        [command] if command == "ensure" => Some(Command::Ensure),
        [command] if command == "status" => Some(Command::Status),
        [command] if matches!(command.as_str(), "help" | "--help" | "-h") => Some(Command::Help),
        _ => None,
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = arguments(std::env::args_os().skip(1));
    let Some(command) = parse_command(&args) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let result = match command {
        Command::Run => runner::run(runner::config_path()).await,
        Command::Mcp => mcp::run(runner::config_path()).await,
        Command::Ensure => ensure::ensure(&runner::config_path()),
        Command::Status => status::print(&runner::config_path()).await,
        Command::Help => {
            println!("{USAGE}");
            Ok(())
        }
    };
    if let Err(error) = result {
        eprintln!("FAIL {error}");
        std::process::exit(1);
    }
}

/// `args_os` rather than `args`, so a crafted argument cannot panic the CLI.
fn arguments(raw: impl Iterator<Item = std::ffi::OsString>) -> Vec<String> {
    raw.map(|arg| arg.to_string_lossy().into_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn each_subcommand_maps_to_one_command() {
        assert_eq!(parse_command(&args(&["run"])), Some(Command::Run));
        assert_eq!(parse_command(&args(&["mcp"])), Some(Command::Mcp));
        assert_eq!(parse_command(&args(&["ensure"])), Some(Command::Ensure));
        assert_eq!(parse_command(&args(&["status"])), Some(Command::Status));
    }

    #[test]
    fn help_is_help_and_anything_else_is_usage() {
        for help in ["help", "--help", "-h"] {
            assert_eq!(parse_command(&args(&[help])), Some(Command::Help), "{help}");
        }
        assert_eq!(parse_command(&args(&[])), None);
        assert_eq!(parse_command(&args(&["bogus"])), None);
        assert_eq!(parse_command(&args(&["run", "extra"])), None);
        assert_eq!(parse_command(&args(&["--help", "run"])), None);
    }

    #[test]
    fn a_non_unicode_argument_is_lossy_and_never_a_command() {
        let args = arguments(std::iter::once(non_unicode_argument()));
        assert_eq!(args.len(), 1);
        assert!(!args[0].is_ascii(), "{}", args[0]);
        assert_eq!(parse_command(&args), None);
    }

    #[cfg(unix)]
    fn non_unicode_argument() -> std::ffi::OsString {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(b"\xff".to_vec())
    }

    #[cfg(windows)]
    fn non_unicode_argument() -> std::ffi::OsString {
        use std::os::windows::ffi::OsStringExt;
        // A lone surrogate is not a Unicode scalar value.
        std::ffi::OsString::from_wide(&[0xD800])
    }
}
