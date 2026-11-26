//! `ntfy-admin`: read the running relay, change topics, stop it, or plan and
//! run a network mode change.
//!
//! Exit codes: 0 ok, 1 refused or failed, 2 usage, 3 no running relay,
//! 4 platform command failed.

use std::path::PathBuf;

use ntfy_relay_admin::cli;
use ntfy_relay_admin::endpoint::LiveEndpoint;
use ntfy_relay_admin::run::{self, EXIT_OK, EXIT_USAGE};

fn main() {
    std::process::exit(dispatch());
}

fn dispatch() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("{}", cli::USAGE);
        return EXIT_USAGE;
    }
    if cli::wants_help(&args) {
        println!("{}", cli::USAGE);
        return EXIT_OK;
    }
    let invocation = match cli::parse(&args) {
        Ok(invocation) => invocation,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("{}", cli::USAGE);
            return EXIT_USAGE;
        }
    };
    let Some(root) = install_root() else {
        eprintln!("FAIL cannot determine the install root");
        return EXIT_USAGE;
    };
    let mut endpoint = LiveEndpoint::new(root);
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    run::execute(
        &invocation.command,
        invocation.json,
        &mut endpoint,
        &mut out,
        &mut err,
    )
}

/// `NTFY_RELAY_HOME` for tests and staging, else the directory that holds the
/// running binary. The CLI is installed beside the relay, so this is the same
/// rule the daemon uses and lands on the install root on both platforms.
fn install_root() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("NTFY_RELAY_HOME") {
        if !value.is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
}
