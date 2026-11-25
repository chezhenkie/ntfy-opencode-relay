//! The live control endpoint, and the seam the command runner is tested on.

use std::path::PathBuf;
use std::process::Command as Process;
use std::thread;
use std::time::{Duration, Instant};

use ntfy_relay_control::client::{Client, ClientError};
use ntfy_relay_control::process::pid_alive;
use ntfy_relay_control::runtime::ControlFile;
use ntfy_relay_control::types::Mode;

/// How long `stop` waits for the daemon to exit, as `12` fixes it.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(50);

pub trait Endpoint {
    fn pid(&mut self) -> Option<u32>;
    fn alive(&mut self, pid: u32) -> bool;
    fn request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), ClientError>;
    fn run_deploy(&mut self, deploy: &str, mode: Mode) -> Result<i32, String>;

    /// Block until the pid exits or the budget runs out.
    fn wait_stopped(&mut self, pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.alive(pid) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(POLL);
        }
    }
}

/// The installed CLI talks to the relay in the same root.
pub struct LiveEndpoint {
    root: PathBuf,
}

impl LiveEndpoint {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Endpoint for LiveEndpoint {
    fn pid(&mut self) -> Option<u32> {
        ControlFile::read(&self.root).map(|file| file.pid)
    }

    fn alive(&mut self, pid: u32) -> bool {
        pid_alive(pid)
    }

    /// A connection per request, so a stale token is dropped by re-reading
    /// the control file on the retry in `run`.
    fn request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), ClientError> {
        Client::connect(&self.root)?.request(method, path, body)
    }

    fn run_deploy(&mut self, deploy: &str, mode: Mode) -> Result<i32, String> {
        let status = Process::new(deploy)
            .arg("mode")
            .arg(mode.as_str())
            .status()
            .map_err(|error| format!("cannot run {deploy}: {error}"))?;
        status
            .code()
            .ok_or_else(|| format!("{deploy} ended without an exit code"))
    }
}
