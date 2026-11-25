//! Command execution seam. Tests inject a fake; the binary uses `SystemRunner`.

use std::process::Command;

use crate::deploy::DeployError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, DeployError>;

    /// Runs a command whose child starts a long-lived process on the same
    /// standard handles. The output is discarded so the caller cannot block on
    /// a pipe the daemon keeps open.
    fn run_quiet(&self, program: &str, args: &[String]) -> Result<CommandOutput, DeployError> {
        self.run(program, args)
    }
}

/// Runs the program directly, never through a shell, so an argument can carry
/// a URL or a quoted value without platform quoting rules.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, DeployError> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|error| DeployError::platform(program, format!("cannot run: {error}")))?;
        Ok(CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn run_quiet(&self, program: &str, args: &[String]) -> Result<CommandOutput, DeployError> {
        use std::process::Stdio;
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let status = command
            .status()
            .map_err(|error| DeployError::platform(program, format!("cannot run: {error}")))?;
        Ok(CommandOutput {
            status: status.code().unwrap_or(-1),
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}
