//! Fakes: no podman runs in CI, and every command is observable.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::auth::AuthError;
use crate::config::Permission;
use crate::runner::{CommandOutput, CommandRunner};

static DIRS: AtomicUsize = AtomicUsize::new(0);

pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ntfy-auth-{tag}-{}-{}",
            std::process::id(),
            DIRS.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&path).expect("create the test directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Default)]
struct State {
    users: Vec<String>,
    admins: Vec<String>,
    rows: Vec<(String, Permission, String)>,
    tokens: Vec<(String, String)>,
    calls: Vec<RecordedCall>,
    fail: Vec<(String, i32)>,
    token_output: bool,
    counter: usize,
}

pub type RecordedCall = (String, Vec<String>, Vec<(String, String)>);

pub struct FakeRunner {
    state: Mutex<State>,
}

impl FakeRunner {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                token_output: true,
                ..State::default()
            }),
        }
    }

    pub fn with_user(name: &str) -> Self {
        let runner = Self::new();
        runner.state.lock().unwrap().users.push(name.to_string());
        runner
    }

    /// A user the listing shows with `role: admin`, which `apply` must refuse.
    pub fn with_admin(name: &str) -> Self {
        let runner = Self::with_user(name);
        runner.state.lock().unwrap().admins.push(name.to_string());
        runner
    }

    pub fn with_access(name: &str, permission: Permission, topic: &str) -> Self {
        let runner = Self::with_user(name);
        runner
            .state
            .lock()
            .unwrap()
            .rows
            .push((name.to_string(), permission, topic.to_string()));
        runner
    }

    pub fn with_token(name: &str, token: &str) -> Self {
        let runner = Self::with_access(name, Permission::Rw, "*");
        runner
            .state
            .lock()
            .unwrap()
            .tokens
            .push((name.to_string(), token.to_string()));
        runner
    }

    /// A `token add` that prints nothing useful, used to prove the parse guard.
    pub fn without_token_output(mut self) -> Self {
        self.state.get_mut().unwrap().token_output = false;
        self
    }

    pub fn fail_on(&self, key: &str, status: i32) {
        self.state
            .lock()
            .unwrap()
            .fail
            .push((key.to_string(), status));
    }

    pub fn calls(&self) -> Vec<RecordedCall> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn keys(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|(program, args, _)| key(program, args))
            .collect()
    }

    pub fn clear_calls(&self) {
        self.state.lock().unwrap().calls.clear();
    }
}

fn key(program: &str, args: &[String]) -> String {
    if program != "podman" {
        return program.to_string();
    }
    let mut index = 1;
    while args.get(index).map(String::as_str) == Some("-e") {
        index += 2;
    }
    match (
        args.get(index + 2).map(String::as_str),
        args.get(index + 3).map(String::as_str),
    ) {
        (Some("user"), Some("list")) => "user list",
        (Some("user"), Some("add")) => "user add",
        (Some("access"), Some(_)) if args.len() == 5 => "access list",
        (Some("access"), Some(_)) => "access add",
        (Some("token"), Some("list")) => "token list",
        (Some("token"), Some("add")) => "token add",
        _ => "other",
    }
    .to_string()
}

impl CommandRunner for &FakeRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<CommandOutput, AuthError> {
        <FakeRunner as CommandRunner>::run(*self, program, args, env)
    }
}

fn arg(args: &[String], index: usize) -> &str {
    args.get(index).map(String::as_str).unwrap_or_default()
}

impl CommandRunner for FakeRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<CommandOutput, AuthError> {
        let key = key(program, args);
        let mut state = self.state.lock().unwrap();
        state
            .calls
            .push((program.to_string(), args.to_vec(), env.to_vec()));
        if let Some((_, status)) = state
            .fail
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
        let stdout = match key.as_str() {
            "user list" => {
                let mut text = String::new();
                for user in &state.users {
                    let role = if state.admins.contains(user) {
                        "admin"
                    } else {
                        "user"
                    };
                    text.push_str(&format!("user {user} (role: {role}, tier: none)\n"));
                    for (name, permission, topic) in &state.rows {
                        if name == user {
                            text.push_str(&format!(
                                "- {} access to topic {topic}\n",
                                permission.access_text()
                            ));
                        }
                    }
                }
                text.push_str("user * (role: anonymous, tier: none)\n");
                text
            }
            "user add" => {
                let user = arg(args, args.len() - 1);
                if !state.users.iter().any(|name| name == user) {
                    state.users.push(user.to_string());
                }
                String::new()
            }
            "access list" => {
                let user = arg(args, 4);
                let mut text = format!("user {user}\n");
                for (name, permission, topic) in &state.rows {
                    if name == user {
                        text.push_str(&format!(
                            "- {} access to topic {topic}\n",
                            permission.access_text()
                        ));
                    }
                }
                text
            }
            "access add" => {
                let user = arg(args, 4).to_string();
                let topic = arg(args, 5).to_string();
                let permission = match arg(args, 6) {
                    "rw" => Permission::Rw,
                    "read" => Permission::Read,
                    "write" => Permission::Write,
                    other => panic!("unexpected permission {other}"),
                };
                state.rows.push((user, permission, topic));
                String::new()
            }
            "token list" => {
                let user = arg(args, 5);
                let mut text = format!("user {user}\n");
                for (name, token) in &state.tokens {
                    if name == user {
                        text.push_str(&format!("- {token} (fake), never expires\n"));
                    }
                }
                text
            }
            "token add" => {
                let user = arg(args, args.len() - 1).to_string();
                state.counter += 1;
                let token = format!("tk_{:026}", state.counter);
                state.tokens.push((user, token.clone()));
                if state.token_output {
                    format!("{token}\n")
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        };
        Ok(CommandOutput {
            status: 0,
            stdout,
            stderr: String::new(),
        })
    }
}
