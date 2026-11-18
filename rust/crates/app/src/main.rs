//! `ntfy-relay run|mcp|status`.
//!
//! One binary for the daemon, the stdio MCP server, and the one-shot checks,
//! so the tool contract and the outbound policy are the same build, as
//! `docs/specsheet/06-rust-crate-split.md` requires.

mod api_client;
mod discovery;
mod lock;
mod logger;
mod mcp;
mod relay;
mod runner;
mod sse;
mod status;
mod store;
mod supervisor;
mod util;

const USAGE: &str = "usage: ntfy-relay run|mcp|status";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [command] if command == "run" => runner::run(runner::config_path()).await,
        [command] if command == "mcp" => mcp::run(runner::config_path()).await,
        [command] if command == "status" => status::print(&runner::config_path()),
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
