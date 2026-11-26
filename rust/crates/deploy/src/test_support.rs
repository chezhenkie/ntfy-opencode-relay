//! Shared fakes: no podman, netsh, or socket runs in CI.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::deploy::{DeployError, HealthProbe};
use crate::runner::{CommandOutput, CommandRunner};

#[derive(Default)]
struct RunnerState {
    calls: Vec<(String, Vec<String>)>,
    inspect: Option<String>,
    image_id: Option<String>,
    show: String,
    failures: Vec<(String, i32)>,
    call_failures: Vec<(String, Vec<String>, i32)>,
    outputs: Vec<(String, String)>,
    missing: Vec<String>,
}

pub struct FakeRunner {
    state: Mutex<RunnerState>,
}

impl FakeRunner {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(RunnerState::default()),
        }
    }

    pub fn with_inspect(text: &str) -> Self {
        let runner = Self::new();
        runner.set_inspect(text);
        runner
    }

    pub fn with_show(text: &str) -> Self {
        let runner = Self::new();
        runner.set_show(text);
        runner
    }

    pub fn set_inspect(&self, text: &str) {
        self.state.lock().unwrap().inspect = Some(text.to_string());
    }

    pub fn set_show(&self, text: &str) {
        self.state.lock().unwrap().show = text.to_string();
    }

    pub fn set_image_id(&self, id: &str) {
        self.state.lock().unwrap().image_id = Some(id.to_string());
    }

    pub fn set_stdout(&self, key: &str, text: &str) {
        self.state
            .lock()
            .unwrap()
            .outputs
            .push((key.to_string(), text.to_string()));
    }

    pub fn fail_on(&self, key: &str, status: i32) {
        self.state
            .lock()
            .unwrap()
            .failures
            .push((key.to_string(), status));
    }

    /// Fail one exact call; other calls to the same program still succeed.
    pub fn fail_on_call(&self, program: &str, args: &[&str], status: i32) {
        self.state.lock().unwrap().call_failures.push((
            program.to_string(),
            args.iter().map(|arg| arg.to_string()).collect(),
            status,
        ));
    }

    /// A program the host does not have: spawn fails like `Command::new` does.
    pub fn without_program(&self, program: &str) {
        self.state.lock().unwrap().missing.push(program.to_string());
    }

    pub fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn count(&self) -> usize {
        self.state.lock().unwrap().calls.len()
    }

    fn key(program: &str, args: &[String]) -> String {
        if matches!(
            program,
            "whoami"
                | "reg"
                | "schtasks"
                | "systemctl"
                | "where"
                | "setx"
                | "powershell"
                | "readlink"
        ) {
            return program.to_string();
        }
        if program == "podman" && args.first().map(String::as_str) == Some("image") {
            return "image".to_string();
        }
        if program == "podman" && args.first().map(String::as_str) == Some("inspect") {
            return "inspect".to_string();
        }
        if program == "netsh" && args.get(2).map(String::as_str) == Some("show") {
            return "show".to_string();
        }
        args.first().cloned().unwrap_or_else(|| program.to_string())
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, DeployError> {
        let key = Self::key(program, args);
        let mut state = self.state.lock().unwrap();
        state.calls.push((program.to_string(), args.to_vec()));
        if state.missing.iter().any(|name| name == program) {
            return Err(DeployError::platform(program, "cannot run: not found"));
        }
        if let Some((_, _, status)) = state
            .call_failures
            .iter()
            .find(|(failed, failed_args, _)| {
                failed == program
                    && failed_args.len() == args.len()
                    && failed_args
                        .iter()
                        .zip(args)
                        .all(|(left, right)| left == right)
            })
            .cloned()
        {
            return Ok(CommandOutput {
                status,
                stdout: String::new(),
                stderr: format!("fake {program} failure"),
            });
        }
        if let Some((_, status)) = state
            .failures
            .iter()
            .find(|(failed, _)| failed == &key)
            .cloned()
        {
            return Ok(CommandOutput {
                status,
                stdout: String::new(),
                stderr: format!("fake {key} failure"),
            });
        }
        if let Some((_, text)) = state
            .outputs
            .iter()
            .find(|(key_name, _)| key_name == &key)
            .cloned()
        {
            return Ok(CommandOutput {
                status: 0,
                stdout: text,
                stderr: String::new(),
            });
        }
        match key.as_str() {
            "inspect" => match &state.inspect {
                Some(text) => Ok(CommandOutput {
                    status: 0,
                    stdout: text.clone(),
                    stderr: String::new(),
                }),
                None => Ok(CommandOutput {
                    status: 125,
                    stdout: String::new(),
                    stderr: "no such container".to_string(),
                }),
            },
            "show" => Ok(CommandOutput {
                status: 0,
                stdout: state.show.clone(),
                stderr: String::new(),
            }),
            "image" => match &state.image_id {
                Some(id) => Ok(CommandOutput {
                    status: 0,
                    stdout: format!("{id}\n"),
                    stderr: String::new(),
                }),
                None => Ok(CommandOutput {
                    status: 125,
                    stdout: String::new(),
                    stderr: "image not known".to_string(),
                }),
            },
            _ => Ok(CommandOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
        }
    }
}

struct ProbeState {
    replies: VecDeque<Result<u16, String>>,
    last: Option<Result<u16, String>>,
}

pub struct FakeProbe {
    state: Mutex<ProbeState>,
}

impl FakeProbe {
    pub fn always(status: u16) -> Self {
        Self::sequence(vec![Ok(status)])
    }

    pub fn sequence(replies: Vec<Result<u16, String>>) -> Self {
        let state = ProbeState {
            replies: replies.into(),
            last: None,
        };
        Self {
            state: Mutex::new(state),
        }
    }
}

impl HealthProbe for FakeProbe {
    fn get(&self, _port: u16) -> Result<u16, String> {
        let mut state = self.state.lock().unwrap();
        let reply = state
            .replies
            .pop_front()
            .or_else(|| state.last.clone())
            .unwrap_or(Ok(500));
        state.last = Some(reply.clone());
        reply
    }
}
