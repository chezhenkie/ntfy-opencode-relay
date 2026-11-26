//! The live control endpoint, and the seam the command runner is tested on.

use std::path::{Path, PathBuf};
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
        let expected = self
            .root
            .join(format!("ntfy-deploy{}", std::env::consts::EXE_SUFFIX));
        if !same_path(Path::new(deploy), &expected) {
            return Err(format!(
                "refusing deploy path {deploy}; expected {}",
                expected.display()
            ));
        }
        let status = Process::new(&expected)
            .arg("mode")
            .arg(mode.as_str())
            .status()
            .map_err(|error| format!("cannot run {}: {error}", expected.display()))?;
        status
            .code()
            .ok_or_else(|| format!("{} ended without an exit code", expected.display()))
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deploy_path_outside_the_root_is_refused_before_any_spawn() {
        let mut endpoint = LiveEndpoint::new(std::env::temp_dir());
        let error = endpoint.run_deploy("/bin/sh", Mode::Nord).unwrap_err();
        assert!(error.contains("refusing deploy path"), "{error}");
    }

    #[test]
    fn the_expected_deploy_name_matches_the_root() {
        let root = std::env::temp_dir();
        let name = format!("ntfy-deploy{}", std::env::consts::EXE_SUFFIX);
        assert!(same_path(&root.join(&name), &root.join(&name)));
        assert!(!same_path(
            &root.join("ntfy-deploy-old.exe"),
            &root.join(&name)
        ));
    }
}
