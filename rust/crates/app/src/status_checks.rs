//! Read-only platform rows for `ntfy-relay status`.
//!
//! This is the reporting half of `tools/startup-check.ps1`: the podman binary
//! and machine, the container, Tailscale Serve, the loopback portproxy, legacy
//! relay files, and loopback health. Nothing is started, repaired, or
//! elevated, and a failed row does not change the exit code.

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ntfy_relay_ntfy::http::{HttpRequest, Method, ReqwestTransport, Transport};
use ntfy_relay_outbound::scope::is_inside;

const HEALTH_TIMEOUT_SECS: u64 = 8;
const HEALTH_MAX_BYTES: u64 = 16 * 1024;
/// A hung podman machine, tailscale daemon, or network stack must not hold
/// `status` open; each platform command gets this long.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_POLL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Linux,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    Skip,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        }
    }
}

/// One report line, in the startup-check shape: verdict, name, detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: &'static str,
    pub verdict: Verdict,
    pub detail: String,
}

impl Row {
    fn new(name: &'static str, verdict: Verdict, detail: impl Into<String>) -> Self {
        Self {
            name,
            verdict,
            detail: detail.into(),
        }
    }

    pub fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Verdict::Pass, detail)
    }

    pub fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Verdict::Fail, detail)
    }

    pub fn skip(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Verdict::Skip, detail)
    }

    pub fn line(&self) -> String {
        if self.detail.is_empty() {
            format!("{} {}", self.verdict.label(), self.name)
        } else {
            format!("{} {} {}", self.verdict.label(), self.name, self.detail)
        }
    }
}

/// Local paths and the URL the checks read. `NTFY_PODMAN_BIN`,
/// `NTFY_TAILSCALE_BIN`, and `NTFY_HEALTH_URL` override the defaults, as the
/// startup check does.
#[derive(Debug, Clone)]
pub struct Tools {
    pub podman: PathBuf,
    pub tailscale: PathBuf,
    pub opencode_dirs: Vec<PathBuf>,
    pub port: u16,
    pub health_url: String,
    pub elevated: bool,
    /// The install root, named by the `acl` row.
    pub root: PathBuf,
    /// `%LOCALAPPDATA%` on Windows, the user's home on Unix: the folder whose
    /// inherited ACLs the root relies on.
    pub profile: Option<PathBuf>,
}

impl Tools {
    pub fn from_env(server: &str) -> Self {
        let server = server.trim_end_matches('/');
        let health_url = env::var("NTFY_HEALTH_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| format!("{server}/v1/health"));
        Self {
            podman: env_program("NTFY_PODMAN_BIN", podman_default()),
            tailscale: env_program("NTFY_TAILSCALE_BIN", tailscale_default()),
            opencode_dirs: opencode_dirs(),
            port: server_port(server),
            health_url,
            elevated: is_elevated(),
            root: ntfy_relay_core::config::install_root().unwrap_or_default(),
            profile: user_profile(),
        }
    }
}

/// The folder whose inherited ACLs the root and its files rely on.
fn user_profile() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "LOCALAPPDATA"
    } else {
        "HOME"
    };
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn env_program(name: &str, fallback: PathBuf) -> PathBuf {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(fallback)
}

fn podman_default() -> PathBuf {
    if cfg!(windows) {
        local_app_data()
            .join("Programs")
            .join("Podman")
            .join("podman.exe")
    } else {
        PathBuf::from("podman")
    }
}

fn tailscale_default() -> PathBuf {
    if cfg!(windows) {
        program_files().join("Tailscale").join("tailscale.exe")
    } else {
        PathBuf::from("tailscale")
    }
}

fn local_app_data() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn program_files() -> PathBuf {
    env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn user_home() -> PathBuf {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn opencode_dirs() -> Vec<PathBuf> {
    let home = user_home();
    vec![
        home.join(".config").join("opencode"),
        home.join(".local").join("state").join("opencode"),
    ]
}

/// Whether the current process holds administrator rights. The agent runs with
/// the daemon's rights, so an elevated relay is reported, never repaired here.
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::Security::{
        AllocateAndInitializeSid, CheckTokenMembership, FreeSid, SECURITY_NT_AUTHORITY,
    };
    const SECURITY_BUILTIN_DOMAIN_RID: u32 = 0x20;
    const DOMAIN_ALIAS_RID_ADMINS: u32 = 0x220;
    let mut sid = std::ptr::null_mut();
    let allocated = unsafe {
        AllocateAndInitializeSid(
            &SECURITY_NT_AUTHORITY,
            2,
            SECURITY_BUILTIN_DOMAIN_RID,
            DOMAIN_ALIAS_RID_ADMINS,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut sid,
        )
    };
    if allocated == 0 {
        return false;
    }
    let mut member: BOOL = 0;
    let checked = unsafe { CheckTokenMembership(std::ptr::null_mut(), sid, &mut member) };
    unsafe {
        FreeSid(sid);
    }
    checked != 0 && member != 0
}

#[cfg(unix)]
pub fn is_elevated() -> bool {
    extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() == 0 }
}

#[cfg(not(any(windows, unix)))]
pub fn is_elevated() -> bool {
    false
}

fn elevated_detail() -> &'static str {
    if cfg!(windows) {
        "running as administrator"
    } else {
        "running as uid 0"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Answer { status: u16, healthy: bool },
    Error(String),
}

/// Report the container health endpoint. Never retries: one GET, one timeout.
pub async fn probe_health(url: &str) -> Health {
    let transport = match ReqwestTransport::new() {
        Ok(transport) => transport,
        Err(error) => return Health::Error(error.to_string()),
    };
    let request = HttpRequest::new(Method::Get, url).max_response_bytes(HEALTH_MAX_BYTES);
    let timeout = Duration::from_secs(HEALTH_TIMEOUT_SECS);
    match tokio::time::timeout(timeout, transport.send(request)).await {
        Ok(Ok(response)) => Health::Answer {
            status: response.status,
            healthy: body_reports_healthy(&response.body),
        },
        Ok(Err(error)) => Health::Error(error.to_string()),
        Err(_) => Health::Error(format!("timeout after {HEALTH_TIMEOUT_SECS}s")),
    }
}

pub fn body_reports_healthy(body: &[u8]) -> bool {
    String::from_utf8_lossy(body).contains("\"healthy\":true")
}

pub trait CommandRunner {
    fn run(&self, program: &Path, args: &[&str]) -> Result<String, String>;
}

pub struct SystemRunner {
    pub timeout: Duration,
}

impl Default for SystemRunner {
    fn default() -> Self {
        Self {
            timeout: COMMAND_TIMEOUT,
        }
    }
}

impl CommandRunner for SystemRunner {
    /// Spawns with piped output, reads both pipes on their own threads, and
    /// polls the child so the deadline is enforced and the child is killed.
    fn run(&self, program: &Path, args: &[&str]) -> Result<String, String> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("{}: {error}", program.display()))?;
        let stdout = child.stdout.take().map(collect_reader);
        let stderr = child.stderr.take().map(collect_reader);
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{}: {error}", program.display()));
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{} timed out after {} s",
                    program.display(),
                    self.timeout.as_secs_f64()
                ));
            }
            thread::sleep(COMMAND_POLL);
        };
        let stdout = stdout
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        let stderr = stderr
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        if status.success() {
            return Ok(String::from_utf8_lossy(&stdout).trim().to_string());
        }
        let code = status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&stderr);
        let detail = stderr.trim();
        if detail.is_empty() {
            Err(format!("{} exit {code}", program.display()))
        } else {
            Err(format!("{} exit {code}: {detail}", program.display()))
        }
    }
}

fn collect_reader<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = reader.read_to_end(&mut buffer);
        buffer
    })
}

/// Every row, in the startup check's order. The runner is a seam for tests;
/// the binary passes `SystemRunner`.
pub fn gather(
    platform: Platform,
    tools: &Tools,
    runner: &dyn CommandRunner,
    health: Health,
) -> Vec<Row> {
    let mut rows = Vec::with_capacity(9);

    let podman_ok = match runner.run(&tools.podman, &["--version"]) {
        Ok(version) => {
            rows.push(Row::pass("podman-binary", version));
            true
        }
        Err(error) => {
            rows.push(Row::fail("podman-binary", error));
            false
        }
    };

    let machine_running = if !podman_ok {
        rows.push(Row::skip("podman-machine", "podman unavailable"));
        false
    } else if platform == Platform::Linux {
        rows.push(Row::pass("podman-machine", "native, no machine"));
        true
    } else {
        let args = ["machine", "list", "--format", "{{.Running}}"];
        match runner.run(&tools.podman, &args) {
            Ok(text) if machine_is_running(&text) => {
                rows.push(Row::pass("podman-machine", "running"));
                true
            }
            Ok(_) => {
                rows.push(Row::fail("podman-machine", "stopped"));
                false
            }
            Err(error) => {
                rows.push(Row::fail("podman-machine", error));
                false
            }
        }
    };

    let mut container_reason = "container not reachable";
    let container_present = if !podman_ok {
        container_reason = "podman unavailable";
        rows.push(Row::skip("container-present", container_reason));
        false
    } else if !machine_running {
        container_reason = "machine stopped";
        rows.push(Row::skip("container-present", container_reason));
        false
    } else {
        let args = [
            "ps",
            "-a",
            "--filter",
            "name=^ntfy$",
            "--format",
            "{{.Names}}",
        ];
        match runner.run(&tools.podman, &args) {
            Ok(text) if container_present(&text, "ntfy") => {
                rows.push(Row::pass("container-present", "ntfy"));
                true
            }
            Ok(_) => {
                container_reason = "container absent";
                rows.push(Row::fail("container-present", "absent"));
                false
            }
            Err(error) => {
                container_reason = "podman ps failed";
                rows.push(Row::fail("container-present", error));
                false
            }
        }
    };

    if !container_present {
        rows.push(Row::skip("container-health", container_reason));
    } else {
        let format = "{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}";
        match runner.run(&tools.podman, &["inspect", "ntfy", "--format", format]) {
            Ok(text) if text == "healthy" => rows.push(Row::pass("container-health", "healthy")),
            Ok(text) if text.is_empty() => rows.push(Row::fail("container-health", "no status")),
            Ok(text) => rows.push(Row::fail("container-health", text)),
            Err(error) => rows.push(Row::fail("container-health", error)),
        }
    }

    let proxy = format!("proxy http://127.0.0.1:{}", tools.port);
    match runner.run(&tools.tailscale, &["serve", "status"]) {
        Ok(text) if serve_has_proxy(&text, tools.port) => {
            rows.push(Row::pass("tailscale-serve", proxy))
        }
        Ok(_) => rows.push(Row::fail("tailscale-serve", proxy)),
        Err(error) => rows.push(Row::fail("tailscale-serve", error)),
    }

    let rule = format!("127.0.0.1 {} ::1 {}", tools.port, tools.port);
    match platform {
        Platform::Windows => {
            let args = ["interface", "portproxy", "show", "v4tov6"];
            match runner.run(Path::new("netsh"), &args) {
                Ok(text) if portproxy_has_loopback(&text, tools.port) => {
                    rows.push(Row::pass("loopback-portproxy", rule))
                }
                Ok(_) => rows.push(Row::fail("loopback-portproxy", rule)),
                Err(error) => rows.push(Row::fail("loopback-portproxy", error)),
            }
        }
        Platform::Linux => rows.push(Row::skip("loopback-portproxy", "no portproxy on linux")),
    }

    let hits = legacy_entries(&tools.opencode_dirs);
    if hits.is_empty() {
        rows.push(Row::pass("relay-root-clean", ""));
    } else {
        rows.push(Row::fail("relay-root-clean", hits.join(",")));
    }

    rows.push(match health {
        Health::Answer {
            status: 200,
            healthy: true,
        } => Row::pass("https-health", &tools.health_url),
        Health::Answer { status, healthy } => Row::fail(
            "https-health",
            format!("{} status={status} healthy={healthy}", tools.health_url),
        ),
        Health::Error(error) => Row::fail("https-health", format!("{} {error}", tools.health_url)),
    });

    if tools.elevated {
        rows.push(Row::fail("elevated", elevated_detail()));
    } else {
        rows.push(Row::pass("elevated", "no"));
    }

    rows.push(acl_row(platform, &tools.root, tools.profile.as_deref()));
    rows
}

/// Windows has no owner-only mode, so the token, configs, and runtime file
/// rely on the root's inherited per-user ACLs. On Unix the files are 0600 and
/// the check is the same precondition. A shared root is reported, not
/// tightened.
fn acl_row(platform: Platform, root: &Path, profile: Option<&Path>) -> Row {
    match profile {
        Some(base) if is_inside(base, root) => {
            let detail = match platform {
                Platform::Windows => format!("{} inherited per-user ACL", root.display()),
                Platform::Linux => format!("{} 0600 files under the user home", root.display()),
            };
            Row::pass("acl", detail)
        }
        Some(base) => Row::fail(
            "acl",
            format!(
                "{} outside {}, no per-user inheritance",
                root.display(),
                base.display()
            ),
        ),
        None => Row::fail("acl", format!("{} has no per-user base", root.display())),
    }
}

/// Port from a base URL, with the scheme default when it carries none.
pub fn server_port(server: &str) -> u16 {
    let (scheme, rest) = match server.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => ("http", server),
    };
    let default = if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let port = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split_once("]:").map(|(_, port)| port)
    } else {
        authority.rsplit_once(':').map(|(_, port)| port)
    };
    port.and_then(|port| port.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .unwrap_or(default)
}

pub fn machine_is_running(text: &str) -> bool {
    text.lines().any(|line| line.trim() == "true")
}

pub fn container_present(text: &str, name: &str) -> bool {
    text.lines().any(|line| line.trim() == name)
}

pub fn serve_has_proxy(text: &str, port: u16) -> bool {
    let needle = format!("proxy http://127.0.0.1:{port}");
    text.lines().any(|line| {
        let Some(index) = line.find(&needle) else {
            return false;
        };
        let rest = &line[index + needle.len()..];
        rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace())
    })
}

pub fn portproxy_has_loopback(text: &str, port: u16) -> bool {
    text.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields.len() == 4
            && fields[0] == "127.0.0.1"
            && fields[1].parse::<u16>() == Ok(port)
            && fields[2] == "::1"
            && fields[3].parse::<u16>() == Ok(port)
    })
}

/// Relay files under an OpenCode directory: the ownership rule forbids them.
pub fn legacy_entries(dirs: &[PathBuf]) -> Vec<String> {
    let mut hits = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("ntfy") || name.starts_with("notification-ntfy") {
                hits.push(entry.path().display().to_string());
            }
        }
    }
    hits.sort();
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntfy_relay_core::test_support::Scratch;

    const SHOW: &str = "
Listen on ipv4:             Connect to ipv6:

Address         Port        Address         Port
--------------- ----------  --------------- ----------
127.0.0.1       8810        ::1             8810
";

    const INSPECT: &str = "podman inspect ntfy --format {{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}";

    struct FakeRunner {
        responses: Vec<(String, Result<String, String>)>,
    }

    impl FakeRunner {
        fn new() -> Self {
            Self {
                responses: Vec::new(),
            }
        }

        fn set(mut self, key: &str, output: &str) -> Self {
            self.responses
                .push((key.to_string(), Ok(output.to_string())));
            self
        }

        fn fail(mut self, key: &str, error: &str) -> Self {
            self.responses
                .push((key.to_string(), Err(error.to_string())));
            self
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &Path, args: &[&str]) -> Result<String, String> {
            let key = format!("{} {}", program.display(), args.join(" "));
            match self.responses.iter().find(|(name, _)| name == &key) {
                Some((_, result)) => result.clone(),
                None => Err(format!("unexpected command: {key}")),
            }
        }
    }

    fn tools(dirs: Vec<PathBuf>) -> Tools {
        Tools {
            podman: PathBuf::from("podman"),
            tailscale: PathBuf::from("tailscale"),
            opencode_dirs: dirs,
            port: 8810,
            health_url: "http://127.0.0.1:8810/v1/health".to_string(),
            elevated: false,
            root: PathBuf::from("/srv/ntfy-relay"),
            profile: Some(PathBuf::from("/srv")),
        }
    }

    fn empty_dir(name: &str) -> Scratch {
        Scratch::new(&format!("app-status-{name}"))
    }

    fn passing_runner() -> FakeRunner {
        FakeRunner::new()
            .set("podman --version", "podman version 5.8.3")
            .set("podman machine list --format {{.Running}}", "true")
            .set(
                "podman ps -a --filter name=^ntfy$ --format {{.Names}}",
                "ntfy",
            )
            .set(INSPECT, "healthy")
            .set(
                "tailscale serve status",
                "https://ntfy.example.ts.net (tailnet only)\n|-- / proxy http://127.0.0.1:8810",
            )
            .set("netsh interface portproxy show v4tov6", SHOW)
    }

    fn row<'a>(rows: &'a [Row], name: &str) -> &'a Row {
        rows.iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("no row {name}"))
    }

    #[test]
    fn a_running_stack_passes_every_row() {
        let dir = empty_dir("all-pass");
        let rows = gather(
            Platform::Windows,
            &tools(vec![dir.path().to_path_buf()]),
            &passing_runner(),
            Health::Answer {
                status: 200,
                healthy: true,
            },
        );
        assert_eq!(rows.len(), 10);
        assert!(
            rows.iter().all(|row| row.verdict == Verdict::Pass),
            "{rows:#?}"
        );
        assert_eq!(
            row(&rows, "container-health").line(),
            "PASS container-health healthy"
        );
        assert_eq!(
            row(&rows, "acl").line(),
            "PASS acl /srv/ntfy-relay inherited per-user ACL"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_stopped_machine_skips_the_container_rows() {
        let dir = empty_dir("stopped");
        let runner = FakeRunner::new()
            .set("podman --version", "podman version 5.8.3")
            .set("podman machine list --format {{.Running}}", "false")
            .set(
                "tailscale serve status",
                "|-- / proxy http://127.0.0.1:8810",
            )
            .set("netsh interface portproxy show v4tov6", SHOW);
        let rows = gather(
            Platform::Windows,
            &tools(vec![dir.path().to_path_buf()]),
            &runner,
            Health::Error("connection refused".to_string()),
        );
        assert_eq!(row(&rows, "podman-machine").verdict, Verdict::Fail);
        assert_eq!(row(&rows, "container-present").verdict, Verdict::Skip);
        assert_eq!(row(&rows, "container-present").detail, "machine stopped");
        assert_eq!(row(&rows, "container-health").verdict, Verdict::Skip);
        assert_eq!(row(&rows, "https-health").verdict, Verdict::Fail);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_podman_skips_the_machine_and_container_rows() {
        let dir = empty_dir("no-podman");
        let runner = FakeRunner::new()
            .fail("podman --version", "podman: not found")
            .set("tailscale serve status", "")
            .set("netsh interface portproxy show v4tov6", "");
        let rows = gather(
            Platform::Windows,
            &tools(vec![dir.path().to_path_buf()]),
            &runner,
            Health::Error("connection refused".to_string()),
        );
        assert_eq!(row(&rows, "podman-binary").verdict, Verdict::Fail);
        assert_eq!(row(&rows, "podman-machine").verdict, Verdict::Skip);
        assert_eq!(row(&rows, "container-present").verdict, Verdict::Skip);
        assert_eq!(row(&rows, "container-health").verdict, Verdict::Skip);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn linux_has_no_machine_and_no_portproxy() {
        let dir = empty_dir("linux");
        let runner = FakeRunner::new()
            .set("podman --version", "podman version 5.8.3")
            .set(
                "podman ps -a --filter name=^ntfy$ --format {{.Names}}",
                "ntfy",
            )
            .set(INSPECT, "healthy")
            .set("tailscale serve status", "proxy http://127.0.0.1:8810");
        let rows = gather(
            Platform::Linux,
            &tools(vec![dir.path().to_path_buf()]),
            &runner,
            Health::Answer {
                status: 200,
                healthy: true,
            },
        );
        assert_eq!(row(&rows, "podman-machine").verdict, Verdict::Pass);
        assert_eq!(row(&rows, "loopback-portproxy").verdict, Verdict::Skip);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_files_fail_the_root_row() {
        let dir = empty_dir("legacy");
        fs::write(dir.join("ntfy-old.json"), "{}").unwrap();
        fs::create_dir_all(dir.join("ntfy-cache")).unwrap();
        let runner = passing_runner();
        let rows = gather(
            Platform::Windows,
            &tools(vec![dir.path().to_path_buf()]),
            &runner,
            Health::Answer {
                status: 200,
                healthy: true,
            },
        );
        assert_eq!(row(&rows, "relay-root-clean").verdict, Verdict::Fail);
        assert!(row(&rows, "relay-root-clean")
            .detail
            .contains("ntfy-old.json"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_entries_ignore_unrelated_names() {
        let dir = empty_dir("legacy-names");
        fs::write(dir.join("opencode.jsonc"), "{}").unwrap();
        fs::write(dir.join("notification-ntfy.json"), "{}").unwrap();
        let hits = legacy_entries(std::slice::from_ref(&dir.path().to_path_buf()));
        assert_eq!(hits.len(), 1);
        assert!(hits[0].ends_with("notification-ntfy.json"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_elevated_relay_fails_the_elevated_row() {
        let dir = empty_dir("elevated");
        let mut elevated = tools(vec![dir.path().to_path_buf()]);
        elevated.elevated = true;
        let rows = gather(
            Platform::Windows,
            &elevated,
            &passing_runner(),
            Health::Answer {
                status: 200,
                healthy: true,
            },
        );
        assert_eq!(row(&rows, "elevated").verdict, Verdict::Fail);
        assert_eq!(row(&rows, "elevated").detail, elevated_detail());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_root_outside_the_profile_fails_the_acl_row() {
        let dir = empty_dir("acl-outside");
        let mut tools = tools(vec![dir.path().to_path_buf()]);
        tools.root = PathBuf::from("C:\\ProgramData\\ntfy-relay");
        tools.profile = Some(PathBuf::from("C:\\Users\\me\\AppData\\Local"));
        let rows = gather(
            Platform::Windows,
            &tools,
            &passing_runner(),
            Health::Answer {
                status: 200,
                healthy: true,
            },
        );
        let acl = row(&rows, "acl");
        assert_eq!(acl.verdict, Verdict::Fail);
        assert!(
            acl.detail.contains("C:\\ProgramData\\ntfy-relay"),
            "{acl:?}"
        );
        assert!(
            acl.detail.contains("C:\\Users\\me\\AppData\\Local"),
            "{acl:?}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_wedged_platform_command_hits_the_deadline() {
        let runner = SystemRunner {
            timeout: Duration::from_millis(200),
        };
        let (program, args) = slow_command();
        let started = Instant::now();
        let error = runner.run(Path::new(program), args).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_successful_command_still_returns_stdout() {
        let runner = SystemRunner::default();
        let (program, args) = echo_command();
        let output = runner.run(Path::new(program), args).unwrap();
        assert_eq!(output, "ok");
    }

    #[cfg(windows)]
    fn slow_command() -> (&'static str, &'static [&'static str]) {
        ("cmd.exe", &["/c", "ping", "-n", "8", "127.0.0.1"])
    }

    #[cfg(target_os = "linux")]
    fn slow_command() -> (&'static str, &'static [&'static str]) {
        ("sleep", &["10"])
    }

    #[cfg(windows)]
    fn echo_command() -> (&'static str, &'static [&'static str]) {
        ("cmd.exe", &["/c", "echo ok"])
    }

    #[cfg(target_os = "linux")]
    fn echo_command() -> (&'static str, &'static [&'static str]) {
        ("echo", &["ok"])
    }

    #[test]
    fn machine_and_container_text_is_matched_exactly() {
        assert!(machine_is_running("true\n"));
        assert!(machine_is_running("true\ntrue"));
        assert!(!machine_is_running("false"));
        assert!(!machine_is_running(""));
        assert!(container_present("other\nntfy\n", "ntfy"));
        assert!(!container_present("ntfy-backup", "ntfy"));
    }

    #[test]
    fn serve_proxy_must_match_the_port_exactly() {
        assert!(serve_has_proxy("|-- / proxy http://127.0.0.1:8810", 8810));
        assert!(!serve_has_proxy("|-- / proxy http://127.0.0.1:88100", 8810));
        assert!(!serve_has_proxy("|-- / proxy http://127.0.0.1:9999", 8810));
        assert!(!serve_has_proxy("", 8810));
    }

    #[test]
    fn portproxy_needs_the_loopback_rule() {
        assert!(portproxy_has_loopback(SHOW, 8810));
        assert!(!portproxy_has_loopback(SHOW, 9999));
        assert!(!portproxy_has_loopback(
            "127.0.0.1 8810 127.0.0.1 8810",
            8810
        ));
        assert!(!portproxy_has_loopback("", 8810));
    }

    #[test]
    fn health_body_matches_the_startup_check() {
        assert!(body_reports_healthy(br#"{"healthy":true}"#));
        assert!(!body_reports_healthy(br#"{"healthy":false}"#));
        assert!(!body_reports_healthy(br#"{"healthy": true}"#));
    }

    #[test]
    fn port_defaults_follow_the_scheme() {
        assert_eq!(server_port("http://127.0.0.1:8810"), 8810);
        assert_eq!(server_port("http://localhost"), 80);
        assert_eq!(server_port("https://ntfy.example.ts.net"), 443);
        assert_eq!(server_port("http://[::1]:9000"), 9000);
        assert_eq!(server_port("https://ntfy.example.ts.net/"), 443);
    }
}
