//! Command execution seam. Tests inject a fake; the binary uses `SystemRunner`.

use std::process::Command;

use crate::auth::AuthError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub trait CommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<CommandOutput, AuthError>;
}

/// Runs the program directly, never through a shell. The password reaches the
/// container through the environment, never through an argument.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<CommandOutput, AuthError> {
        let output = Command::new(program)
            .args(args)
            .envs(env.iter().map(|(key, value)| (key, value)))
            .output()
            .map_err(|error| AuthError::platform(program, format!("cannot run: {error}")))?;
        Ok(CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
