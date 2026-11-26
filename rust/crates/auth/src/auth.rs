//! Apply and status: the container CLI is the only writer.
//!
//! The sequence is read, then create only what is absent: the user, the
//! wildcard row, and a token that is written to the install root. A second run
//! issues no command that changes state and never rewrites a valid token file.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;

use crate::config::{AuthConfig, ConfigError, Permission};
use crate::runner::{CommandOutput, CommandRunner};

const PODMAN: &str = "podman";
const PASSWORD_ENV: &str = "NTFY_PASSWORD";

#[derive(Debug)]
pub enum AuthError {
    Config(ConfigError),
    Platform {
        program: String,
        detail: String,
    },
    Command {
        action: String,
        status: i32,
        stderr: String,
    },
    Io {
        detail: String,
    },
}

impl AuthError {
    pub fn platform(program: &str, detail: impl Into<String>) -> Self {
        Self::Platform {
            program: program.to_string(),
            detail: detail.into(),
        }
    }

    fn command(action: &str, output: &CommandOutput) -> Self {
        Self::Command {
            action: action.to_string(),
            status: output.status,
            stderr: excerpt(&output.stderr),
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Config(_) => 2,
            Self::Platform { .. } | Self::Command { .. } | Self::Io { .. } => 4,
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => f.write_str(&error.to_string()),
            Self::Platform { program, detail } => write!(f, "{program}: {detail}"),
            Self::Command {
                action,
                status,
                stderr,
            } => {
                if stderr.is_empty() {
                    write!(f, "{action}: exit {status}")
                } else {
                    write!(f, "{action}: exit {status}: {stderr}")
                }
            }
            Self::Io { detail } => f.write_str(detail),
        }
    }
}

impl std::error::Error for AuthError {}

fn excerpt(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut short: String = line.chars().take(200).collect();
    if short.len() < line.len() {
        short.push_str("...");
    }
    short
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Created,
    Kept,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub user: Change,
    pub access: Change,
    pub token: Change,
    pub dry_run: bool,
    pub plan: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenState {
    Listed,
    NotListed,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub user: bool,
    pub access: bool,
    pub token: TokenState,
}

impl StatusReport {
    pub fn ok(&self) -> bool {
        self.user && self.access && self.token == TokenState::Listed
    }
}

struct Snapshot {
    user: bool,
    /// The role text of an existing configured user, when the listing names it.
    role: Option<String>,
    access: bool,
    tokens: Vec<String>,
    file_token: Option<String>,
}

pub struct AuthEngine<R: CommandRunner> {
    runner: R,
}

impl<R: CommandRunner> AuthEngine<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    pub fn apply(
        &self,
        config: &AuthConfig,
        token_path: &Path,
        dry_run: bool,
    ) -> Result<Report, AuthError> {
        let snapshot = self.inspect(config, token_path)?;
        let mut plan = Vec::new();
        let mut user = Change::Kept;
        let mut access = Change::Kept;
        let mut token = Change::Kept;

        if let Some(role) = snapshot.role.as_deref() {
            if !role.is_empty() && !role.eq_ignore_ascii_case("user") {
                return Err(AuthError::Config(ConfigError::new(format!(
                    "user {} exists with role {role}; expected user",
                    config.user
                ))));
            }
        }
        if !snapshot.user {
            user = Change::Created;
            plan.push(format!(
                "podman exec -e {PASSWORD_ENV}=<random> {} ntfy user add --role=user {}",
                config.container, config.user
            ));
            if !dry_run {
                let password = random_password();
                let env = vec![(PASSWORD_ENV.to_string(), password)];
                self.exec(
                    config,
                    "user add",
                    &["user", "add", "--role=user", &config.user],
                    &env,
                )?;
            }
        }

        if !snapshot.access {
            access = Change::Created;
            plan.push(format!(
                "podman exec {} ntfy access {} \"{}\" {}",
                config.container,
                config.user,
                config.topic,
                config.permission.as_str()
            ));
            if !dry_run {
                self.exec(
                    config,
                    "access",
                    &[
                        "access",
                        &config.user,
                        &config.topic,
                        config.permission.as_str(),
                    ],
                    &[],
                )?;
            }
        }

        let token_listed = snapshot
            .file_token
            .as_ref()
            .is_some_and(|value| snapshot.tokens.iter().any(|held| held == value));
        if !token_listed {
            token = Change::Created;
            plan.push(format!(
                "podman exec {} ntfy token add --label={} {}",
                config.container, config.token_label, config.user
            ));
            plan.push(format!("write {}", config.token_file));
            if !dry_run {
                let output = self.exec(
                    config,
                    "token add",
                    &[
                        "token",
                        "add",
                        &format!("--label={}", config.token_label),
                        &config.user,
                    ],
                    &[],
                )?;
                let value = parse_token_value(&output.stdout).ok_or_else(|| AuthError::Io {
                    detail: format!("token add: output has no token for user {}", config.user),
                })?;
                write_token_file(token_path, &value)?;
            }
        }

        Ok(Report {
            user,
            access,
            token,
            dry_run,
            plan,
        })
    }

    pub fn status(
        &self,
        config: &AuthConfig,
        token_path: &Path,
    ) -> Result<StatusReport, AuthError> {
        let snapshot = self.inspect(config, token_path)?;
        let token = match &snapshot.file_token {
            None => TokenState::Missing,
            Some(value) if snapshot.tokens.iter().any(|held| held == value) => TokenState::Listed,
            Some(_) => TokenState::NotListed,
        };
        Ok(StatusReport {
            user: snapshot.user,
            access: snapshot.access,
            token,
        })
    }

    fn inspect(&self, config: &AuthConfig, token_path: &Path) -> Result<Snapshot, AuthError> {
        let users = self
            .exec(config, "user list", &["user", "list"], &[])?
            .stdout;
        let rows = parse_users(&users);
        let role = rows
            .iter()
            .find(|row| row.name == config.user)
            .map(|row| row.role.clone());
        let user = role.is_some();
        let (access, tokens) = if user {
            let rows = self
                .exec(config, "access", &["access", &config.user], &[])?
                .stdout;
            let access = parse_access(&rows).iter().any(|(permission, topic)| {
                *permission == config.permission && topic == &config.topic
            });
            let tokens = self
                .exec(config, "token list", &["token", "list", &config.user], &[])?
                .stdout;
            (access, parse_tokens(&tokens))
        } else {
            (false, Vec::new())
        };
        Ok(Snapshot {
            user,
            role,
            access,
            tokens,
            file_token: read_token_file(token_path)?,
        })
    }

    fn exec(
        &self,
        config: &AuthConfig,
        action: &str,
        rest: &[&str],
        env: &[(String, String)],
    ) -> Result<CommandOutput, AuthError> {
        let mut args = vec!["exec".to_string()];
        // Podman forwards only the variables named with -e from its own
        // environment; the value never enters the argument list.
        for (name, _value) in env {
            args.push("-e".to_string());
            args.push(name.clone());
        }
        args.push(config.container.clone());
        args.push("ntfy".to_string());
        args.extend(rest.iter().map(|value| value.to_string()));
        let output = self.runner.run(PODMAN, &args, env)?;
        if output.status != 0 {
            return Err(AuthError::command(action, &output));
        }
        Ok(output)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UserRow {
    name: String,
    role: String,
}

fn parse_users(text: &str) -> Vec<UserRow> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("user ")?;
            let name = rest.split_whitespace().next()?.to_string();
            let role = rest
                .split_once("(role:")
                .and_then(|(_, tail)| tail.split([',', ')']).next())
                .map(str::trim)
                .unwrap_or_default()
                .to_string();
            Some(UserRow { name, role })
        })
        .collect()
}

fn parse_access(text: &str) -> Vec<(Permission, String)> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("- ")?;
            let (phrase, topic) = rest.split_once(" access to topic ")?;
            let permission = match phrase {
                "read-write" => Permission::Rw,
                "read-only" => Permission::Read,
                "write-only" => Permission::Write,
                _ => return None,
            };
            Some((permission, topic.trim().to_string()))
        })
        .collect()
}

fn parse_tokens(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("- ")?;
            let value = rest.split_whitespace().next()?;
            is_token(value).then(|| value.to_string())
        })
        .collect()
}

fn parse_token_value(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|value| is_token(value))
        .map(str::to_string)
}

fn is_token(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("tk_") else {
        return false;
    };
    (8..=64).contains(&rest.len()) && rest.chars().all(|ch| ch.is_ascii_alphanumeric())
}

fn read_token_file(path: &Path) -> Result<Option<String>, AuthError> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let value = text.trim();
            Ok(is_token(value).then(|| value.to_string()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AuthError::Io {
            detail: format!("token file {}: {error}", path.display()),
        }),
    }
}

fn write_token_file(path: &Path, token: &str) -> Result<(), AuthError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| AuthError::Io {
        detail: format!("token file {}: {error}", path.display()),
    })?;
    file.write_all(token.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|error| AuthError::Io {
            detail: format!("token file {}: {error}", path.display()),
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            AuthError::Io {
                detail: format!("token file {}: {error}", path.display()),
            }
        })?;
    }
    Ok(())
}

/// A 32-character password that is never stored or used. `RandomState` is
/// seeded from OS entropy, so the value cannot be predicted from the clock.
fn random_password() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut value = String::with_capacity(32);
    for round in 0..2 {
        let mut hasher = RandomState::new().build_hasher();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|span| span.as_nanos())
            .unwrap_or(0);
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        hasher.write_u8(round);
        value.push_str(&format!("{:016x}", hasher.finish()));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeRunner, TempDir};

    fn config() -> AuthConfig {
        AuthConfig::parse_text(
            r#"{"version":1,"container":"ntfy","user":"phone","topic":"*","permission":"rw","tokenLabel":"owner","tokenFile":"ntfy-token.txt"}"#,
        )
        .unwrap()
    }

    fn token_file(dir: &TempDir) -> std::path::PathBuf {
        dir.path().join("ntfy-token.txt")
    }

    /// A well-formed fake token; the seed keeps the cases readable while the
    /// source carries no literal that resembles a real token.
    fn fake_token(seed: &str) -> String {
        format!("tk_{seed:0>26}")
    }

    fn mutating(keys: &[String]) -> Vec<&String> {
        keys.iter()
            .filter(|key| key.ends_with("add"))
            .collect::<Vec<_>>()
    }

    #[test]
    fn first_apply_creates_user_row_and_token() {
        let dir = TempDir::new("first");
        let runner = FakeRunner::new();
        let engine = AuthEngine::new(&runner);
        let report = engine.apply(&config(), &token_file(&dir), false).unwrap();
        assert_eq!(report.user, Change::Created);
        assert_eq!(report.access, Change::Created);
        assert_eq!(report.token, Change::Created);
        assert_eq!(report.plan.len(), 4);
        assert!(!report.plan.iter().any(|line| line.contains("tk_")));

        let keys = runner.keys();
        assert_eq!(
            keys,
            vec!["user list", "user add", "access add", "token add"]
        );
        let calls = runner.calls();
        let password = calls[1].2.first().expect("password env");
        assert_eq!(password.0, PASSWORD_ENV);
        assert_eq!(password.1.len(), 32);
        let call = &calls[1];
        assert!(
            call.1
                .windows(2)
                .any(|pair| pair[0] == "-e" && pair[1] == PASSWORD_ENV),
            "exec args: {:?}",
            call.1
        );
        assert!(
            call.1.iter().all(|arg| arg != &password.1),
            "password reached the argument list"
        );
        let text = std::fs::read_to_string(token_file(&dir)).unwrap();
        assert!(is_token(text.trim()), "{text}");
        assert!(!text.contains(password.1.as_str()));
    }

    #[test]
    fn second_apply_is_a_no_op() {
        let dir = TempDir::new("second");
        let runner = FakeRunner::new();
        let engine = AuthEngine::new(&runner);
        engine.apply(&config(), &token_file(&dir), false).unwrap();
        runner.clear_calls();

        let report = engine.apply(&config(), &token_file(&dir), false).unwrap();
        assert_eq!(report.user, Change::Kept);
        assert_eq!(report.access, Change::Kept);
        assert_eq!(report.token, Change::Kept);
        assert!(mutating(&runner.keys()).is_empty());
        assert_eq!(
            runner.keys(),
            vec!["user list", "access list", "token list"]
        );
    }

    #[test]
    fn an_existing_admin_user_is_refused() {
        let dir = TempDir::new("admin");
        let runner = FakeRunner::with_admin("phone");
        let engine = AuthEngine::new(&runner);
        let error = engine
            .apply(&config(), &token_file(&dir), false)
            .unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(
            error
                .to_string()
                .contains("user phone exists with role admin; expected user"),
            "{error}"
        );
        assert!(mutating(&runner.keys()).is_empty(), "{:?}", runner.keys());
    }

    #[test]
    fn a_valid_token_file_is_kept() {
        let dir = TempDir::new("keep");
        let token = fake_token("keep");
        let runner = FakeRunner::with_token("phone", &token);
        std::fs::write(token_file(&dir), format!("{token}\n")).unwrap();

        let report = AuthEngine::new(&runner)
            .apply(&config(), &token_file(&dir), false)
            .unwrap();
        assert_eq!(report.token, Change::Kept);
        assert!(mutating(&runner.keys()).is_empty());
    }

    #[test]
    fn a_missing_token_file_is_restored() {
        let dir = TempDir::new("restore");
        let runner = FakeRunner::with_access("phone", Permission::Rw, "*");
        let report = AuthEngine::new(&runner)
            .apply(&config(), &token_file(&dir), false)
            .unwrap();
        assert_eq!(report.token, Change::Created);
        assert_eq!(
            runner.keys(),
            vec!["user list", "access list", "token list", "token add"]
        );
        assert!(token_file(&dir).is_file());
    }

    #[test]
    fn a_stale_file_token_is_replaced() {
        let dir = TempDir::new("stale");
        let live = fake_token("live");
        let gone = fake_token("gone");
        let runner = FakeRunner::with_token("phone", &live);
        std::fs::write(token_file(&dir), format!("{gone}\n")).unwrap();
        let report = AuthEngine::new(&runner)
            .apply(&config(), &token_file(&dir), false)
            .unwrap();
        assert_eq!(report.token, Change::Created);
        let written = std::fs::read_to_string(token_file(&dir)).unwrap();
        assert_ne!(written.trim(), gone);
        assert!(is_token(written.trim()));
    }

    #[test]
    fn dry_run_changes_nothing() {
        let dir = TempDir::new("dry");
        let runner = FakeRunner::new();
        let report = AuthEngine::new(&runner)
            .apply(&config(), &token_file(&dir), true)
            .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.user, Change::Created);
        assert_eq!(report.token, Change::Created);
        assert_eq!(runner.keys(), vec!["user list"]);
        assert!(!token_file(&dir).exists());
        let plan = report.plan.join("\n");
        assert!(plan.contains("NTFY_PASSWORD=<random>"), "{plan}");
        assert!(plan.contains("ntfy access phone \"*\" rw"), "{plan}");
        assert!(plan.contains("write ntfy-token.txt"), "{plan}");
    }

    #[test]
    fn dry_run_reports_a_complete_state_as_kept() {
        let dir = TempDir::new("dry-kept");
        let token = fake_token("keep");
        let runner = FakeRunner::with_token("phone", &token);
        std::fs::write(token_file(&dir), format!("{token}\n")).unwrap();
        let report = AuthEngine::new(&runner)
            .apply(&config(), &token_file(&dir), true)
            .unwrap();
        assert_eq!(report.user, Change::Kept);
        assert_eq!(report.access, Change::Kept);
        assert_eq!(report.token, Change::Kept);
        assert!(report.plan.is_empty());
        assert!(mutating(&runner.keys()).is_empty());
    }

    #[test]
    fn status_is_ok_when_all_three_exist() {
        let dir = TempDir::new("status-ok");
        let token = fake_token("keep");
        let runner = FakeRunner::with_token("phone", &token);
        std::fs::write(token_file(&dir), format!("{token}\n")).unwrap();
        let report = AuthEngine::new(&runner)
            .status(&config(), &token_file(&dir))
            .unwrap();
        assert!(report.ok());
        assert_eq!(report.token, TokenState::Listed);
    }

    #[test]
    fn status_reports_a_missing_user_and_file() {
        let dir = TempDir::new("status-empty");
        let runner = FakeRunner::new();
        let report = AuthEngine::new(&runner)
            .status(&config(), &token_file(&dir))
            .unwrap();
        assert!(!report.ok());
        assert!(!report.user);
        assert!(!report.access);
        assert_eq!(report.token, TokenState::Missing);
        assert!(!token_file(&dir).exists());
        assert_eq!(runner.keys(), vec!["user list"]);
    }

    #[test]
    fn status_flags_a_file_token_the_server_does_not_hold() {
        let dir = TempDir::new("status-stale");
        let live = fake_token("live");
        let gone = fake_token("gone");
        let runner = FakeRunner::with_token("phone", &live);
        std::fs::write(token_file(&dir), format!("{gone}\n")).unwrap();
        let report = AuthEngine::new(&runner)
            .status(&config(), &token_file(&dir))
            .unwrap();
        assert!(!report.ok());
        assert_eq!(report.token, TokenState::NotListed);
    }

    #[test]
    fn a_command_failure_is_a_platform_error() {
        let dir = TempDir::new("fail");
        let runner = FakeRunner::new();
        runner.fail_on("user add", 125);
        let error = AuthEngine::new(&runner)
            .apply(&config(), &token_file(&dir), false)
            .unwrap_err();
        assert_eq!(error.exit_code(), 4);
        assert!(error.to_string().contains("user add"), "{error}");
        assert!(!token_file(&dir).exists());
    }

    #[test]
    fn token_add_output_is_required() {
        let dir = TempDir::new("no-token");
        let runner = FakeRunner::with_access("phone", Permission::Rw, "*").without_token_output();
        let error = AuthEngine::new(&runner)
            .apply(&config(), &token_file(&dir), false)
            .unwrap_err();
        assert_eq!(error.exit_code(), 4);
        assert!(error.to_string().contains("no token"), "{error}");
        assert!(!token_file(&dir).exists());
    }

    #[test]
    fn an_unwritable_token_file_is_an_io_error() {
        let dir = TempDir::new("unwritable");
        let path = dir.path().join("missing").join("ntfy-token.txt");
        let runner = FakeRunner::new();
        let error = AuthEngine::new(&runner)
            .apply(&config(), &path, false)
            .unwrap_err();
        assert_eq!(error.exit_code(), 4);
        assert!(error.to_string().contains("token file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("mode");
        let runner = FakeRunner::new();
        AuthEngine::new(&runner)
            .apply(&config(), &token_file(&dir), false)
            .unwrap();
        let mode = std::fs::metadata(token_file(&dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn parse_users_reads_the_cli_table() {
        let text = "user phone (role: user, tier: none)\n\
                    - read-write access to topic *\n\
                    user * (role: anonymous, tier: none)\n";
        let rows = parse_users(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "phone");
        assert_eq!(rows[0].role, "user");
        assert_eq!(rows[1].name, "*");
        assert_eq!(rows[1].role, "anonymous");
    }

    #[test]
    fn parse_access_reads_each_row() {
        let text = "user phone\n\
                    - read-write access to topic *\n\
                    - read-only access to topic proj\n";
        assert_eq!(
            parse_access(text),
            vec![
                (Permission::Rw, "*".to_string()),
                (Permission::Read, "proj".to_string())
            ]
        );
        assert!(parse_access("user phone\n- no topic-specific permissions\n").is_empty());
    }

    #[test]
    fn parse_tokens_reads_token_lines() {
        let token = fake_token("a12345678");
        let text = format!(
            "user phone\n\
             - {token} (owner), never expires, accessed from ::1\n"
        );
        assert_eq!(parse_tokens(&text), vec![token.clone()]);
        assert_eq!(
            parse_token_value(&format!("token: {token}\n")).unwrap(),
            token
        );
        assert!(parse_token_value("nothing here").is_none());
    }

    #[test]
    fn token_shape_is_checked() {
        assert!(is_token(&fake_token("a12345678")));
        assert!(!is_token("tk_"));
        assert!(!is_token("tk_short"));
        assert!(!is_token("xx_a12345678"));
        assert!(!is_token("tk_has-dash"));
    }

    #[test]
    fn random_passwords_are_hex_and_differ() {
        let first = random_password();
        let second = random_password();
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
