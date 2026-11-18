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
}
