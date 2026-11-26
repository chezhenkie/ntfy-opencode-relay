//! One-shot `status` output for `startup-check.ps1` and manual checks.
//!
//! Reads the config, lock, state, log tail, and the read-only platform rows
//! from `status_checks`. It never writes, starts, or repairs anything, and a
//! failed row does not change the exit code.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use ntfy_relay_control::process::pid_alive;
use ntfy_relay_core::config::load_config;
use ntfy_relay_core::state::read_state;

use crate::lock::{lock_path_for, read_lock_body};
use crate::status_checks;

const LOG_TAIL_BYTES: u64 = 4096;

pub async fn print(config_path: &Path) -> Result<(), String> {
    let config = load_config(config_path).map_err(|error| error.to_string())?;
    println!("config {} ok", config_path.display());

    let lock_file = lock_path_for(&config.state_file);
    match read_lock_body(&lock_file) {
        Ok(Some(body)) => println!(
            "lock {} pid={} {} phase={}",
            lock_file.display(),
            body.pid,
            if pid_alive(body.pid) { "alive" } else { "dead" },
            body.phase
        ),
        Ok(None) => println!("lock {} missing", lock_file.display()),
        Err(error) => println!("lock {} unreadable: {error}", lock_file.display()),
    }

    match read_state(&config.state_file) {
        Ok(state) => {
            let pending = state
                .targets
                .values()
                .filter(|target| target.pending.is_some())
                .count();
            println!(
                "state targets={} pending={} topics={}",
                state.targets.len(),
                pending,
                state.topics.len()
            );
        }
        Err(error) => println!("state {} unreadable: {error}", config.state_file.display()),
    }

    match tail_line(&config.logging.file, LOG_TAIL_BYTES) {
        Ok(Some(line)) => println!("log {line}"),
        Ok(None) => println!("log {} empty", config.logging.file.display()),
        Err(error) => println!("log {} unreadable: {error}", config.logging.file.display()),
    }

    let tools = status_checks::Tools::from_env(&config.ntfy.server);
    let health = status_checks::probe_health(&tools.health_url).await;
    let rows = status_checks::gather(
        status_checks::Platform::current(),
        &tools,
        &status_checks::SystemRunner::default(),
        health,
    );
    for row in rows {
        println!("{}", row.line());
    }
    Ok(())
}

fn tail_line(file: &Path, limit: u64) -> std::io::Result<Option<String>> {
    let mut handle = match fs::File::open(file) {
        Ok(handle) => handle,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let size = handle.metadata()?.len();
    let start = size.saturating_sub(limit);
    handle.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::new();
    handle.take(limit).read_to_end(&mut buffer)?;
    let text = String::from_utf8_lossy(&buffer);
    Ok(text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::to_string))
}
