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

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [command] if command == "run" => runner::run(runner::config_path()).await,
        [command] if command == "mcp" => mcp::run(runner::config_path()).await,
        [command] if command == "ensure" => ensure::ensure(&runner::config_path()),
        [command] if command == "status" => status::print(&runner::config_path()).await,
        [command] if matches!(command.as_str(), "help" | "--help" | "-h") => {
            println!("{USAGE}");
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(error) = result {
        eprintln!("FAIL {error}");
        std::process::exit(1);
    }
}
