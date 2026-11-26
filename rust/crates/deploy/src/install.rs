//! `ntfy-deploy install|remove`: place the four executables, write the config
//! defaults that are missing, register the watchdog and startup entries, and
//! undo all of it without touching the token or the state.
//!
//! The installer normalizes the host, not the product: it starts and stops the
//! relay through the installed binaries and never writes `ntfy-deploy.json`.

use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::ConfigError;
use crate::deploy::DeployError;
use crate::platform::Platform;
use crate::runner::CommandRunner;

pub const WATCHDOG_TASK: &str = "ntfy-relay-watchdog";
pub const STARTUP_TASK: &str = "ntfy-relay-startup";
pub const STARTUP_SCRIPT: &str = "ntfy-relay-startup.vbs";
pub const ENV_NAME: &str = "NTFY_RELAY_HOME";
pub const SHORTCUT_NAME: &str = "ntfy startup check.lnk";
pub const RELAY_CONFIG: &str = "ntfy-relay.json";
pub const AUTH_CONFIG: &str = "ntfy-auth.json";

/// The four executables, without the platform suffix.
pub const BINARIES: [&str; 4] = ["ntfy-relay", "ntfy-deploy", "ntfy-auth", "ntfy-admin"];

/// Highest numbered previous name tried when a running image holds the
/// canonical one. Each number is reused once the file it named is free.
const MAX_PREVIOUS: u32 = 9;

const RELAY_CONFIG_TEXT: &str = "{\n  \"ntfy\": {\n    \"server\": \"http://127.0.0.1:8810\"\n  },\n  \"projectRoots\": [],\n  \"outbound\": {\n    \"enabled\": true\n  }\n}\n";
const AUTH_CONFIG_TEXT: &str = "{\n  \"version\": 1,\n  \"container\": \"ntfy\",\n  \"user\": \"phone\",\n  \"topic\": \"*\",\n  \"permission\": \"rw\",\n  \"tokenLabel\": \"owner\",\n  \"tokenFile\": \"ntfy-token.txt\"\n}\n";

/// Directories the installer needs besides the root and its own image.
#[derive(Debug, Clone)]
pub struct InstallPaths {
    pub root: PathBuf,
    pub source: PathBuf,
    /// Windows: the `%APPDATA%` folder that holds the legacy startup shortcut.
    pub app_data: Option<PathBuf>,
    /// Linux: the systemd user unit folder.
    pub unit_dir: Option<PathBuf>,
    /// `%LOCALAPPDATA%` on Windows, the user's home on Unix: the folder whose
    /// inherited ACLs the root and its files rely on.
    pub user_base: Option<PathBuf>,
}

impl InstallPaths {
    pub fn shortcut(&self) -> Option<PathBuf> {
        self.app_data.as_ref().map(|base| {
            base.join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
                .join("Startup")
                .join(SHORTCUT_NAME)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub lines: Vec<String>,
}

impl InstallReport {
    fn push(&mut self, line: String) {
        self.lines.push(line);
    }
}

pub fn binary_name(platform: Platform, stem: &str) -> String {
    match platform {
        Platform::Windows => format!("{stem}.exe"),
        Platform::Linux => stem.to_string(),
    }
}

/// Default install root: the override, else `%LOCALAPPDATA%\ntfy-relay` on
/// Windows and `~/.local/share/ntfy-relay` on Linux. No home-directory guess
/// beyond that.
pub fn default_root(
    platform: Platform,
    override_value: Option<&std::ffi::OsStr>,
    local_app_data: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(value) = override_value.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(value));
    }
    match platform {
        Platform::Windows => local_app_data
            .filter(|value| !value.is_empty())
            .map(|base| PathBuf::from(base).join("ntfy-relay")),
        Platform::Linux => home.filter(|value| !value.is_empty()).map(|base| {
            PathBuf::from(base)
                .join(".local")
                .join("share")
                .join("ntfy-relay")
        }),
    }
}

/// The per-user folder whose inherited ACLs the root and its files rely on:
/// `%LOCALAPPDATA%` on Windows, `HOME` on Unix.
pub fn per_user_base(platform: Platform) -> Option<PathBuf> {
    let name = match platform {
        Platform::Windows => "LOCALAPPDATA",
        Platform::Linux => "HOME",
    };
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Windows has no owner-only mode, so the token, configs, and runtime file
/// rely on the root's inherited per-user ACLs. A root outside the per-user
/// folder is reported, not tightened; a shared root is the operator's choice.
pub fn acl_warning(platform: Platform, root: &Path, base: Option<&Path>) -> Option<String> {
    let base = base?;
    if is_under(platform, root, base) {
        return None;
    }
    Some(format!(
        "warning: root is outside {}; the token and configs rely on inherited per-user ACLs",
        base.display()
    ))
}

/// Whole-component containment, with the platform's case rule.
fn is_under(platform: Platform, root: &Path, base: &Path) -> bool {
    let parts = |path: &Path| -> Vec<String> {
        path.components()
            .map(|part| {
                let text = part.as_os_str().to_string_lossy().into_owned();
                if platform == Platform::Windows {
                    text.to_lowercase()
                } else {
                    text
                }
            })
            .collect()
    };
    let root = parts(root);
    let base = parts(base);
    root.len() >= base.len() && root[..base.len()] == base[..]
}

pub fn install(
    runner: &dyn CommandRunner,
    platform: Platform,
    paths: &InstallPaths,
    dry_run: bool,
) -> Result<InstallReport, DeployError> {
    let mut report = InstallReport { lines: Vec::new() };
    report.push(format!("root: {}", paths.root.display()));
    if let Some(warning) = acl_warning(platform, &paths.root, paths.user_base.as_deref()) {
        report.push(warning);
    }
    report.push(format!("source: {}", paths.source.display()));

    let files = check_release_files(paths, platform)?;
    if !dry_run {
        fs::create_dir_all(&paths.root).map_err(|error| DeployError::Io {
            detail: format!("install: cannot create {}: {error}", paths.root.display()),
        })?;
    }

    for name in &files {
        let target = paths.root.join(name);
        let source = paths.source.join(name);
        if same_bytes(&source, &target).map_err(|error| DeployError::Io {
            detail: format!("install: cannot compare {name}: {error}"),
        })? {
            report.push(format!("install: kept {name}"));
            continue;
        }
        let previous = previous_name(platform, name);
        if dry_run {
            report.push(format!("install: would place {name}"));
            continue;
        }
        let kept = place(&source, &target, &paths.root.join(&previous)).map_err(|error| {
            DeployError::Io {
                detail: format!("install: cannot place {name}: {error}"),
            }
        })?;
        let kept = kept
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or(previous);
        report.push(format!("install: placed {name}, previous {kept}"));
    }

    for (name, text) in [
        (RELAY_CONFIG, RELAY_CONFIG_TEXT),
        (AUTH_CONFIG, AUTH_CONFIG_TEXT),
    ] {
        let path = paths.root.join(name);
        if path.exists() {
            report.push(format!("config: kept {name}"));
            continue;
        }
        if dry_run {
            report.push(format!("config: would write {name}"));
            continue;
        }
        fs::write(&path, text).map_err(|error| DeployError::Io {
            detail: format!("config: cannot write {}: {error}", path.display()),
        })?;
        report.push(format!("config: written {name}"));
    }

    let autostart = if platform == Platform::Windows {
        install_windows(runner, paths, dry_run, &mut report)?;
        true
    } else {
        install_linux(runner, paths, dry_run, &mut report)?
    };

    if dry_run {
        report.push("relay: would restart".to_string());
    } else {
        stop_relay(runner, platform, &paths.root, &mut report)?;
        start_relay(runner, platform, &paths.root, &mut report)?;
    }
    report.push(match (dry_run, autostart) {
        (true, _) => "result: planned".to_string(),
        (false, true) => "result: installed".to_string(),
        (false, false) => "result: installed, without autostart".to_string(),
    });
    Ok(report)
}

pub fn remove(
    runner: &dyn CommandRunner,
    platform: Platform,
    paths: &InstallPaths,
    dry_run: bool,
) -> Result<InstallReport, DeployError> {
    let mut report = InstallReport { lines: Vec::new() };
    report.push(format!("root: {}", paths.root.display()));

    if platform == Platform::Windows {
        if dry_run {
            report.push("relay: would stop".to_string());
        } else {
            stop_relay(runner, platform, &paths.root, &mut report)?;
        }
        remove_windows(runner, paths, dry_run, &mut report)?;
    } else {
        if dry_run {
            report.push("relay: would stop".to_string());
        } else {
            stop_relay(runner, platform, &paths.root, &mut report)?;
        }
        remove_linux(runner, paths, dry_run, &mut report)?;
    }

    for stem in BINARIES {
        let name = binary_name(platform, stem);
        let previous = previous_name(platform, &name);
        let target = paths.root.join(&name);
        let staged = staged_path(&target);
        if staged.exists() {
            let label = staged
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if dry_run {
                report.push(format!("remove: would delete {label}"));
            } else {
                match fs::remove_file(&staged) {
                    Ok(()) => report.push(format!("remove: deleted {label}")),
                    Err(error) => report.push(format!("remove: kept {label}, {error}")),
                }
            }
        }
        if target.exists() {
            if dry_run {
                report.push(format!("remove: would delete {name}"));
            } else {
                fs::remove_file(&target).map_err(|error| DeployError::Io {
                    detail: format!("remove: cannot delete {name}: {error}"),
                })?;
                report.push(format!("remove: deleted {name}"));
            }
        }
        let canonical = paths.root.join(&previous);
        let mut copies = vec![canonical.clone()];
        for index in 2..=MAX_PREVIOUS {
            copies.push(numbered_previous(&canonical, index));
        }
        for path in copies {
            if !path.exists() {
                continue;
            }
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or(previous.clone());
            if dry_run {
                report.push(format!("remove: would delete {label}"));
                continue;
            }
            // A previous image can be the running file of an MCP process that
            // outlived its install; it cannot be deleted and is not an error.
            match fs::remove_file(&path) {
                Ok(()) => report.push(format!("remove: deleted {label}")),
                Err(error) => report.push(format!("remove: kept {label}, {error}")),
            }
        }
    }

    report.push("kept: configs, token, state, asks, journal, log".to_string());
    report.push(if dry_run {
        "result: planned".to_string()
    } else {
        "result: removed".to_string()
    });
    Ok(report)
}

fn check_release_files(
    paths: &InstallPaths,
    platform: Platform,
) -> Result<Vec<String>, DeployError> {
    let mut names = Vec::with_capacity(BINARIES.len());
    for stem in BINARIES {
        let name = binary_name(platform, stem);
        if !paths.source.join(&name).is_file() {
            return Err(DeployError::Config(ConfigError::new(format!(
                "install: {name} is missing beside the installer"
            ))));
        }
        names.push(name);
    }
    Ok(names)
}

/// `<stem>.previous.exe` for `ntfy-relay.exe`, `<stem>.previous` on Linux.
pub fn previous_name(platform: Platform, name: &str) -> String {
    match platform {
        Platform::Windows => format!("{}.previous.exe", name.trim_end_matches(".exe")),
        Platform::Linux => format!("{name}.previous"),
    }
}

fn same_bytes(left: &Path, right: &Path) -> std::io::Result<bool> {
    let left_meta = match fs::metadata(left) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let right_meta = match fs::metadata(right) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if left_meta.len() != right_meta.len() {
        return Ok(false);
    }
    let mut left_file = fs::File::open(left)?;
    let mut right_file = fs::File::open(right)?;
    let mut left_buf = [0u8; 64 * 1024];
    let mut right_buf = [0u8; 64 * 1024];
    loop {
        let left_read = left_file.read(&mut left_buf)?;
        let right_read = right_file.read(&mut right_buf)?;
        if left_read != right_read {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
        if left_buf[..left_read] != right_buf[..right_read] {
            return Ok(false);
        }
    }
}

/// Stage the new image beside the target, then rename it over the target. A
/// failed copy restores the outgoing image and removes the staged file, so the
/// target always holds a complete binary. A running MCP process can hold the
/// canonical previous name as its own image; that file cannot be deleted, so
/// the outgoing image moves to the next free numbered name instead. The
/// returned path is the file that now holds the outgoing image.
fn place(source: &Path, target: &Path, previous: &Path) -> std::io::Result<PathBuf> {
    let mut restored = None;
    let mut moved_to = previous.to_path_buf();
    if target.exists() {
        moved_to = free_previous(previous)?;
        fs::rename(target, &moved_to)?;
        restored = Some(moved_to.clone());
    }
    let staged = staged_path(target);
    let result = fs::copy(source, &staged).and_then(|_| fs::rename(&staged, target));
    match result {
        Ok(()) => Ok(moved_to),
        Err(error) => {
            let _ = fs::remove_file(&staged);
            if let Some(previous) = restored {
                let _ = fs::rename(previous, target);
            }
            Err(error)
        }
    }
}

/// `<target>.new` beside the target, on the same volume as the rename.
fn staged_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!("{name}.new"))
}

/// The previous name when it is free, else the first free `<name>.N` variant.
fn free_previous(previous: &Path) -> std::io::Result<PathBuf> {
    if !previous.exists() {
        return Ok(previous.to_path_buf());
    }
    if fs::remove_file(previous).is_ok() {
        return Ok(previous.to_path_buf());
    }
    for index in 2..=MAX_PREVIOUS {
        let alt = numbered_previous(previous, index);
        if !alt.exists() {
            return Ok(alt);
        }
        if fs::remove_file(&alt).is_ok() {
            return Ok(alt);
        }
    }
    Err(std::io::Error::other(format!(
        "{} and its numbered copies up to {MAX_PREVIOUS} are in use",
        previous.display()
    )))
}

/// `ntfy-relay.previous.exe` becomes `ntfy-relay.previous.2.exe`, and the
/// Linux name without a suffix gets the same treatment.
fn numbered_previous(previous: &Path, index: u32) -> PathBuf {
    let name = previous
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let numbered = match name.strip_suffix(".exe") {
        Some(stem) => format!("{stem}.{index}.exe"),
        None => format!("{name}.{index}"),
    };
    previous.with_file_name(numbered)
}

fn stop_relay(
    runner: &dyn CommandRunner,
    platform: Platform,
    root: &Path,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    let pid = endpoint_pid(root);
    let program = root.join(binary_name(platform, "ntfy-admin"));
    let output = runner.run(&program.to_string_lossy(), &["stop".to_string()])?;
    // 0 stopped, 1 still running after the CLI budget, 3 no running relay.
    if !matches!(output.status, 0 | 1 | 3) {
        return Err(DeployError::platform(
            "ntfy-admin",
            format!("stop exited {}: {}", output.status, output.stderr.trim()),
        ));
    }
    if output.status != 1 {
        // A clean stop needs no kill; exit 3 means no relay answered, so the
        // endpoint pid cannot be trusted.
        return Ok(());
    }
    let Some(pid) = pid else {
        report.push("stop: not killed, no endpoint pid".to_string());
        return Ok(());
    };
    if pid_runs_root_relay(runner, platform, root, pid) {
        force_kill(runner, platform, pid);
    } else {
        report.push("stop: not killed, endpoint stale".to_string());
    }
    Ok(())
}

/// True when the pid still runs the relay image under this install root. A
/// stale endpoint file can name any process, and a force kill of an unrelated
/// process is worse than leaving a stuck relay for the operator.
fn pid_runs_root_relay(
    runner: &dyn CommandRunner,
    platform: Platform,
    root: &Path,
    pid: u32,
) -> bool {
    let output = match platform {
        Platform::Windows => runner.run(
            "powershell",
            &[
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                format!("(Get-Process -Id {pid} -ErrorAction SilentlyContinue).Path"),
            ],
        ),
        Platform::Linux => runner.run("readlink", &["-f".to_string(), format!("/proc/{pid}/exe")]),
    };
    let output = match output {
        Ok(output) if output.status == 0 => output,
        _ => return false,
    };
    let expected = root.join(binary_name(platform, "ntfy-relay"));
    same_path(Path::new(output.stdout.trim()), &expected)
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

/// The endpoint file names the daemon that answered the stop. A process still
/// alive after the CLI's 15 s budget is not shutting down; kill it so a second
/// daemon cannot take the lock beside it.
fn force_kill(runner: &dyn CommandRunner, platform: Platform, pid: u32) {
    let pid = pid.to_string();
    let _ = match platform {
        Platform::Windows => runner.run("taskkill", &["/F".to_string(), "/PID".to_string(), pid]),
        Platform::Linux => runner.run("kill", &["-9".to_string(), pid]),
    };
}

pub fn endpoint_pid(root: &Path) -> Option<u32> {
    let text = fs::read_to_string(root.join("ntfy-relay-endpoint.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("pid")?
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
}

fn start_relay(
    runner: &dyn CommandRunner,
    platform: Platform,
    root: &Path,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    let program = root.join(binary_name(platform, "ntfy-relay"));
    let output = runner.run_quiet(&program.to_string_lossy(), &["ensure".to_string()])?;
    if output.status != 0 {
        return Err(DeployError::platform(
            "ntfy-relay",
            format!("ensure exited {}: {}", output.status, output.stderr.trim()),
        ));
    }
    let line = output.stdout.lines().next().unwrap_or("started").trim();
    report.push(format!("relay: {line}"));
    Ok(())
}

fn install_windows(
    runner: &dyn CommandRunner,
    paths: &InstallPaths,
    dry_run: bool,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    let sid = read_sid(runner)?;
    let podman = read_podman(runner)?;
    let root = &paths.root;
    write_startup_script(root, &podman, dry_run, report)?;
    let watchdog = task_xml_watchdog(root, &sid);
    let startup = task_xml_startup(root, &sid, &system_wscript());
    register_task(runner, WATCHDOG_TASK, &watchdog, root, dry_run, report)?;
    register_task(runner, STARTUP_TASK, &startup, root, dry_run, report)?;
    set_env(runner, root, dry_run, report)?;
    remove_shortcut(paths, dry_run, report)?;
    remove_scripts(&paths.root, "relay-windows-scripts", dry_run, report)?;
    Ok(())
}

fn remove_windows(
    runner: &dyn CommandRunner,
    paths: &InstallPaths,
    dry_run: bool,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    for name in [WATCHDOG_TASK, STARTUP_TASK] {
        delete_task(runner, name, dry_run, report)?;
    }
    remove_startup_script(&paths.root, dry_run, report)?;
    remove_shortcut(paths, dry_run, report)?;
    remove_env(runner, dry_run, report)?;
    remove_scripts(&paths.root, "relay-windows-scripts", dry_run, report)?;
    Ok(())
}

fn install_linux(
    runner: &dyn CommandRunner,
    paths: &InstallPaths,
    dry_run: bool,
    report: &mut InstallReport,
) -> Result<bool, DeployError> {
    let Some(unit_dir) = &paths.unit_dir else {
        return Err(DeployError::Config(ConfigError::new(
            "install: no systemd user directory is known",
        )));
    };
    let state = systemd_state(runner);
    if state != SystemdState::Ready {
        report.push(unit_skip_line(state).to_string());
        report.push(
            "warning: no autostart and no watchdog, run ntfy-relay ensure after each boot"
                .to_string(),
        );
        return Ok(false);
    }
    let units = [
        (
            "ntfy-relay-watchdog.service",
            unit_watchdog_service(&paths.root),
        ),
        ("ntfy-relay-watchdog.timer", unit_watchdog_timer()),
        (
            "ntfy-relay-startup.service",
            unit_startup_service(&paths.root),
        ),
    ];
    for (name, text) in units {
        let path = unit_dir.join(name);
        if dry_run {
            report.push(format!("unit: would write {name}"));
            continue;
        }
        fs::create_dir_all(unit_dir).map_err(|error| DeployError::Io {
            detail: format!("unit: cannot create {}: {error}", unit_dir.display()),
        })?;
        fs::write(&path, text).map_err(|error| DeployError::Io {
            detail: format!("unit: cannot write {}: {error}", path.display()),
        })?;
        report.push(format!("unit: written {name}"));
    }
    for args in [
        vec!["--user", "daemon-reload"],
        vec!["--user", "enable", "--now", "ntfy-relay-watchdog.timer"],
        vec!["--user", "enable", "ntfy-relay-startup.service"],
    ] {
        run_platform(runner, "systemctl", &args, dry_run, report)?;
    }
    remove_scripts(&paths.root, "relay-linux-scripts", dry_run, report)?;
    Ok(true)
}

fn remove_linux(
    runner: &dyn CommandRunner,
    paths: &InstallPaths,
    dry_run: bool,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    let Some(unit_dir) = &paths.unit_dir else {
        return Err(DeployError::Config(ConfigError::new(
            "remove: no systemd user directory is known",
        )));
    };
    let systemd = systemd_state(runner);
    if systemd == SystemdState::Ready {
        for args in [
            vec!["--user", "disable", "--now", "ntfy-relay-watchdog.timer"],
            vec!["--user", "disable", "ntfy-relay-startup.service"],
        ] {
            run_platform(runner, "systemctl", &args, dry_run, report)?;
        }
    } else {
        report.push(unit_skip_line(systemd).to_string());
    }
    for name in [
        "ntfy-relay-watchdog.service",
        "ntfy-relay-watchdog.timer",
        "ntfy-relay-startup.service",
    ] {
        let path = unit_dir.join(name);
        if !path.exists() {
            continue;
        }
        if dry_run {
            report.push(format!("unit: would delete {name}"));
            continue;
        }
        fs::remove_file(&path).map_err(|error| DeployError::Io {
            detail: format!("unit: cannot delete {name}: {error}"),
        })?;
        report.push(format!("unit: deleted {name}"));
    }
    if systemd == SystemdState::Ready {
        run_platform(
            runner,
            "systemctl",
            &["--user", "daemon-reload"],
            dry_run,
            report,
        )?;
    }
    remove_scripts(&paths.root, "relay-linux-scripts", dry_run, report)?;
    Ok(())
}

/// What the host can do with systemd user units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemdState {
    Ready,
    Missing,
    NoUserSession,
}

/// A host without systemd cannot take the user units, and a host whose user
/// manager is not running cannot enable them: `systemctl --version` answers
/// there, but every `--user` call fails. Both probes are read-only, so a dry
/// run reports the same plan as a real run.
fn systemd_state(runner: &dyn CommandRunner) -> SystemdState {
    match runner.run("systemctl", &["--version".to_string()]) {
        Ok(output) if output.status == 0 => {}
        _ => return SystemdState::Missing,
    }
    match runner.run(
        "systemctl",
        &["--user".to_string(), "show-environment".to_string()],
    ) {
        Ok(output) if output.status == 0 => SystemdState::Ready,
        _ => SystemdState::NoUserSession,
    }
}

fn unit_skip_line(state: SystemdState) -> &'static str {
    match state {
        SystemdState::NoUserSession => {
            "unit: skipped, no systemd user session (systemctl --user show-environment failed)"
        }
        _ => "unit: skipped, no systemd (systemctl not found)",
    }
}

fn run_platform(
    runner: &dyn CommandRunner,
    program: &str,
    args: &[&str],
    dry_run: bool,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    if dry_run {
        report.push(format!("command: {program} {}", args.join(" ")));
        return Ok(());
    }
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let output = runner.run(program, &args)?;
    if output.status != 0 {
        return Err(DeployError::platform(
            program,
            format!(
                "{} exited {}: {}",
                args.join(" "),
                output.status,
                output.stderr.trim()
            ),
        ));
    }
    Ok(())
}

/// `whoami /user /fo csv /nh`: the account line ends with the SID.
pub fn parse_sid(text: &str) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    let sid = line.rsplit(',').next()?.trim().trim_matches('"');
    sid.starts_with("S-1-").then(|| sid.to_string())
}

pub fn parse_where(text: &str) -> Option<PathBuf> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from)
}

fn read_sid(runner: &dyn CommandRunner) -> Result<String, DeployError> {
    let args: Vec<String> = ["/user", "/fo", "csv", "/nh"]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let output = runner.run("whoami", &args)?;
    if output.status != 0 {
        return Err(DeployError::platform(
            "whoami",
            format!("exited {}: {}", output.status, output.stderr.trim()),
        ));
    }
    parse_sid(&output.stdout).ok_or_else(|| {
        DeployError::platform("whoami", "the account line carries no SID".to_string())
    })
}

fn read_podman(runner: &dyn CommandRunner) -> Result<PathBuf, DeployError> {
    let output = runner.run("where", &["podman".to_string()])?;
    if output.status != 0 {
        return Err(DeployError::Config(ConfigError::new(
            "install: podman is not on PATH",
        )));
    }
    parse_where(&output.stdout)
        .ok_or_else(|| DeployError::Config(ConfigError::new("install: podman is not on PATH")))
}

fn register_task(
    runner: &dyn CommandRunner,
    name: &str,
    xml: &str,
    root: &Path,
    dry_run: bool,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    if dry_run {
        report.push(format!("task: would register {name}"));
        return Ok(());
    }
    let file = root.join(format!("{name}.xml"));
    fs::write(&file, encode_task(xml)).map_err(|error| DeployError::Io {
        detail: format!("task: cannot write {}: {error}", file.display()),
    })?;
    let args = vec![
        "/Create".to_string(),
        "/TN".to_string(),
        name.to_string(),
        "/XML".to_string(),
        file.to_string_lossy().into_owned(),
        "/F".to_string(),
    ];
    let output = runner.run("schtasks", &args);
    let _ = fs::remove_file(&file);
    let output = output?;
    if output.status != 0 {
        return Err(DeployError::platform(
            "schtasks",
            format!(
                "/Create /TN {name} exited {}: {}",
                output.status,
                output.stderr.trim()
            ),
        ));
    }
    report.push(format!("task: registered {name}"));
    Ok(())
}

fn delete_task(
    runner: &dyn CommandRunner,
    name: &str,
    dry_run: bool,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    let query = runner.run(
        "schtasks",
        &["/Query".to_string(), "/TN".to_string(), name.to_string()],
    )?;
    if query.status != 0 {
        report.push(format!("task: absent {name}"));
        return Ok(());
    }
    if dry_run {
        report.push(format!("task: would delete {name}"));
        return Ok(());
    }
    let args = vec![
        "/Delete".to_string(),
        "/TN".to_string(),
        name.to_string(),
        "/F".to_string(),
    ];
    let output = runner.run("schtasks", &args)?;
    if output.status != 0 {
        return Err(DeployError::platform(
            "schtasks",
            format!(
                "/Delete /TN {name} exited {}: {}",
                output.status,
                output.stderr.trim()
            ),
        ));
    }
    report.push(format!("task: deleted {name}"));
    Ok(())
}

fn env_is_set(runner: &dyn CommandRunner, root: &Path) -> Result<bool, DeployError> {
    let args = vec![
        "query".to_string(),
        r"HKCU\Environment".to_string(),
        "/v".to_string(),
        ENV_NAME.to_string(),
    ];
    let output = runner.run("reg", &args)?;
    if output.status != 0 {
        return Ok(false);
    }
    let Some(value) = registry_value(&output.stdout, ENV_NAME) else {
        return Ok(false);
    };
    let wanted = root.to_string_lossy();
    Ok(value
        .split(';')
        .any(|entry| entry.trim().eq_ignore_ascii_case(&wanted)))
}

/// The data column of one `reg query /v` line. The value may carry spaces, so
/// only the name and type are split off; a sibling prefix never matches.
fn registry_value<'a>(stdout: &'a str, name: &str) -> Option<&'a str> {
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        let mut parts = trimmed.split_whitespace();
        if parts.next() != Some(name) {
            continue;
        }
        let kind = parts.next()?;
        if !kind.starts_with("REG_") {
            continue;
        }
        let rest = trimmed.strip_prefix(name)?;
        let offset = name.len() + rest.find(kind)? + kind.len();
        return Some(trimmed[offset..].trim());
    }
    None
}

fn set_env(
    runner: &dyn CommandRunner,
    root: &Path,
    dry_run: bool,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    if env_is_set(runner, root)? {
        report.push(format!("env: kept {ENV_NAME}"));
        return Ok(());
    }
    if dry_run {
        report.push(format!("env: would set {ENV_NAME}"));
        return Ok(());
    }
    let args = vec![ENV_NAME.to_string(), root.to_string_lossy().into_owned()];
    let output = runner.run("setx", &args)?;
    if output.status != 0 {
        return Err(DeployError::platform(
            "setx",
            format!("exited {}: {}", output.status, output.stderr.trim()),
        ));
    }
    report.push(format!("env: set {ENV_NAME}"));
    Ok(())
}

fn remove_env(
    runner: &dyn CommandRunner,
    dry_run: bool,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    let args = vec![
        "query".to_string(),
        r"HKCU\Environment".to_string(),
        "/v".to_string(),
        ENV_NAME.to_string(),
    ];
    let output = runner.run("reg", &args)?;
    if output.status != 0 {
        report.push(format!("env: absent {ENV_NAME}"));
        return Ok(());
    }
    if dry_run {
        report.push(format!("env: would remove {ENV_NAME}"));
        return Ok(());
    }
    let args = vec![
        "delete".to_string(),
        r"HKCU\Environment".to_string(),
        "/v".to_string(),
        ENV_NAME.to_string(),
        "/f".to_string(),
    ];
    let output = runner.run("reg", &args)?;
    if output.status != 0 {
        return Err(DeployError::platform(
            "reg",
            format!(
                "delete {ENV_NAME} exited {}: {}",
                output.status,
                output.stderr.trim()
            ),
        ));
    }
    report.push(format!("env: removed {ENV_NAME}"));
    Ok(())
}

fn remove_shortcut(
    paths: &InstallPaths,
    dry_run: bool,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    let Some(path) = paths.shortcut() else {
        return Ok(());
    };
    if !path.exists() {
        report.push("shortcut: absent".to_string());
        return Ok(());
    }
    if dry_run {
        report.push("shortcut: would remove".to_string());
        return Ok(());
    }
    fs::remove_file(&path).map_err(|error| DeployError::Io {
        detail: format!("shortcut: cannot delete {}: {error}", path.display()),
    })?;
    report.push("shortcut: removed".to_string());
    Ok(())
}

/// `schtasks /Create /XML` accepts UTF-16 LE with a BOM; without it the parser
/// rejects the declaration and reports the file as malformed.
pub fn encode_task(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + text.len() * 2);
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

fn remove_scripts(
    root: &Path,
    folder: &str,
    dry_run: bool,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    let path = root.join(folder);
    if !path.exists() {
        report.push(format!("scripts: absent {folder}"));
        return Ok(());
    }
    if dry_run {
        report.push(format!("scripts: would remove {folder}"));
        return Ok(());
    }
    fs::remove_dir_all(&path).map_err(|error| DeployError::Io {
        detail: format!("scripts: cannot delete {}: {error}", path.display()),
    })?;
    report.push(format!("scripts: removed {folder}"));
    Ok(())
}

fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

fn task_header(description: &str, name: &str, sid: &str, trigger: &str, actions: &str) -> String {
    let mut xml = String::new();
    let _ = write!(
        xml,
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
  <RegistrationInfo>\n    <Description>{description}</Description>\n    <URI>\\{name}</URI>\n  </RegistrationInfo>\n\
  <Principals>\n    <Principal id=\"Author\">\n      <UserId>{sid}</UserId>\n      <LogonType>InteractiveToken</LogonType>\n      <RunLevel>LeastPrivilege</RunLevel>\n    </Principal>\n  </Principals>\n\
  <Settings>\n    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n    <StartWhenAvailable>true</StartWhenAvailable>\n    <ExecutionTimeLimit>PT10M</ExecutionTimeLimit>\n    <Hidden>true</Hidden>\n  </Settings>\n\
  <Triggers>{trigger}</Triggers>\n\
  <Actions Context=\"Author\">{actions}</Actions>\n\
</Task>\n"
    );
    xml
}

fn exec_action(program: &Path, arguments: &str, root: &Path) -> String {
    format!(
        "<Exec><Command>{}</Command><Arguments>{}</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec>",
        xml_escape(&program.to_string_lossy()),
        xml_escape(arguments),
        xml_escape(&root.to_string_lossy())
    )
}

pub fn task_xml_watchdog(root: &Path, sid: &str) -> String {
    let relay = root.join("ntfy-relay.exe");
    let trigger = "<TimeTrigger><StartBoundary>2020-01-01T00:00:00</StartBoundary><Repetition><Interval>PT5M</Interval></Repetition></TimeTrigger>";
    task_header(
        "Restarts the ntfy inbox relay when its lock owner is gone.",
        WATCHDOG_TASK,
        sid,
        trigger,
        &exec_action(&relay, "ensure", root),
    )
}

/// The logon chain runs through `wscript.exe` because Podman and the relay are
/// console programs, and a task action that names one directly flashes a
/// console window. Style 0 creates the child console hidden. The Podman exit
/// code is ignored: `machine start` reports an already running machine as an
/// error. The container returns through `unless-stopped` and the restart unit;
/// `apply` belongs to a deploy, not to boot.
pub fn startup_script_text(root: &Path, podman: &Path) -> String {
    let podman = podman.display();
    let relay = root.join("ntfy-relay.exe");
    let relay = relay.display();
    format!(
        "' Generated by ntfy-deploy install. Runs the logon chain without console windows.\r\n\
Option Explicit\r\n\
Dim sh, code\r\n\
Set sh = CreateObject(\"WScript.Shell\")\r\n\
sh.Run \"\"\"{podman}\"\" machine start\", 0, True\r\n\
code = sh.Run(\"\"\"{relay}\"\" ensure\", 0, True)\r\n\
WScript.Quit code\r\n"
    )
}

fn write_startup_script(
    root: &Path,
    podman: &Path,
    dry_run: bool,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    if dry_run {
        report.push(format!("script: would write {STARTUP_SCRIPT}"));
        return Ok(());
    }
    let path = root.join(STARTUP_SCRIPT);
    fs::write(&path, startup_script_text(root, podman)).map_err(|error| DeployError::Io {
        detail: format!("script: cannot write {}: {error}", path.display()),
    })?;
    report.push(format!("script: written {STARTUP_SCRIPT}"));
    Ok(())
}

fn remove_startup_script(
    root: &Path,
    dry_run: bool,
    report: &mut InstallReport,
) -> Result<(), DeployError> {
    let path = root.join(STARTUP_SCRIPT);
    if !path.exists() {
        report.push(format!("script: absent {STARTUP_SCRIPT}"));
        return Ok(());
    }
    if dry_run {
        report.push(format!("script: would delete {STARTUP_SCRIPT}"));
        return Ok(());
    }
    fs::remove_file(&path).map_err(|error| DeployError::Io {
        detail: format!("script: cannot delete {}: {error}", path.display()),
    })?;
    report.push(format!("script: deleted {STARTUP_SCRIPT}"));
    Ok(())
}

fn system_wscript() -> PathBuf {
    let base = std::env::var_os("SystemRoot")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    base.join("System32").join("wscript.exe")
}

pub fn task_xml_startup(root: &Path, sid: &str, wscript: &Path) -> String {
    let script = root.join(STARTUP_SCRIPT);
    let arguments = format!("//B //NoLogo \"{}\"", script.display());
    let trigger =
        format!("<LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger>");
    task_header(
        "Starts the Podman machine and the relay at logon.",
        STARTUP_TASK,
        sid,
        &trigger,
        &exec_action(wscript, &arguments, root),
    )
}

fn unit_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn unit_header(description: &str, root: &Path) -> String {
    format!(
        "[Unit]\nDescription={description}\n\n[Service]\nType=oneshot\nWorkingDirectory={}\n",
        unit_path(root)
    )
}

pub fn unit_watchdog_service(root: &Path) -> String {
    format!(
        "{}ExecStart=\"{}\" ensure\n",
        unit_header("Restart the ntfy relay when its lock owner is gone.", root),
        unit_path(&root.join("ntfy-relay"))
    )
}

pub fn unit_watchdog_timer() -> String {
    "[Unit]\nDescription=Run the ntfy relay watchdog every five minutes\n\n[Timer]\nOnBootSec=1min\nOnUnitActiveSec=5min\nUnit=ntfy-relay-watchdog.service\n\n[Install]\nWantedBy=timers.target\n".to_string()
}

pub fn unit_startup_service(root: &Path) -> String {
    format!(
        "{}ExecStart=\"{}\" ensure\n\n[Install]\nWantedBy=default.target\n",
        unit_header("Start the ntfy relay at login.", root),
        unit_path(&root.join("ntfy-relay"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FakeRunner;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join("ntfy-deploy-install-tests");
            let dir = base.join(format!("{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn release(scratch: &Scratch, bytes: &[u8]) -> (PathBuf, PathBuf) {
        let source = scratch.path().join("release");
        let root = scratch.path().join("root");
        fs::create_dir_all(&source).unwrap();
        for stem in BINARIES {
            fs::write(source.join(format!("{stem}.exe")), bytes).unwrap();
        }
        (source, root)
    }

    fn paths(source: PathBuf, root: PathBuf, scratch: &Scratch) -> InstallPaths {
        InstallPaths {
            root,
            source,
            app_data: Some(scratch.path().join("appdata")),
            unit_dir: None,
            user_base: None,
        }
    }

    fn windows_runner() -> FakeRunner {
        let runner = FakeRunner::new();
        runner.set_stdout("whoami", "\"BOX\\user\",\"S-1-5-21-1-2-3-1001\"\n");
        runner.set_stdout("where", "C:\\Program Files\\Podman\\podman.exe\n");
        runner.set_stdout("ensure", "running pid=42\n");
        runner.set_stdout("stop", "");
        runner
    }

    #[test]
    fn binary_names_follow_the_platform() {
        assert_eq!(
            binary_name(Platform::Windows, "ntfy-relay"),
            "ntfy-relay.exe"
        );
        assert_eq!(binary_name(Platform::Linux, "ntfy-relay"), "ntfy-relay");
        assert_eq!(
            previous_name(Platform::Windows, "ntfy-relay.exe"),
            "ntfy-relay.previous.exe"
        );
        assert_eq!(
            previous_name(Platform::Linux, "ntfy-relay"),
            "ntfy-relay.previous"
        );
    }

    #[test]
    fn acl_containment_follows_the_platform_case_rule() {
        assert!(is_under(
            Platform::Windows,
            Path::new("c:/work/appdata/local/ntfy-relay"),
            Path::new("C:/Work/AppData/Local")
        ));
        assert!(!is_under(
            Platform::Windows,
            Path::new("C:/Work/app-other/ntfy-relay"),
            Path::new("C:/Work/app")
        ));
        assert!(!is_under(
            Platform::Linux,
            Path::new("/srv/app-other/ntfy-relay"),
            Path::new("/srv/app")
        ));
    }

    #[test]
    fn a_root_outside_the_per_user_folder_is_reported() {
        let scratch = Scratch::new("acl-root");
        let (source, root) = release(&scratch, b"one");
        let mut paths = paths(source, root, &scratch);
        paths.user_base = Some(scratch.path().to_path_buf());
        let inside = install(&windows_runner(), Platform::Windows, &paths, true).unwrap();
        assert!(!inside
            .lines
            .iter()
            .any(|line| line.starts_with("warning: root is outside")));

        paths.user_base = Some(scratch.path().join("profile"));
        let outside = install(&windows_runner(), Platform::Windows, &paths, true).unwrap();
        assert!(outside
            .lines
            .iter()
            .any(|line| line.starts_with("warning: root is outside")));
    }

    #[test]
    fn the_default_root_follows_the_platform() {
        let overlay = std::ffi::OsStr::new(r"C:\custom");
        assert_eq!(
            default_root(
                Platform::Windows,
                Some(overlay),
                Some(std::ffi::OsStr::new(r"C:\la")),
                None
            ),
            Some(PathBuf::from(overlay))
        );
        assert_eq!(
            default_root(
                Platform::Windows,
                None,
                Some(std::ffi::OsStr::new(r"C:\la")),
                None
            ),
            Some(PathBuf::from(r"C:\la").join("ntfy-relay"))
        );
        assert_eq!(
            default_root(
                Platform::Linux,
                None,
                None,
                Some(std::ffi::OsStr::new("/home/user"))
            ),
            Some(
                PathBuf::from("/home/user")
                    .join(".local")
                    .join("share")
                    .join("ntfy-relay")
            )
        );
        assert_eq!(default_root(Platform::Windows, None, None, None), None);
    }

    #[test]
    fn a_missing_release_file_refuses_before_any_write() {
        let scratch = Scratch::new("missing");
        let (source, root) = release(&scratch, b"one");
        fs::remove_file(source.join("ntfy-auth.exe")).unwrap();
        let runner = windows_runner();
        let error = install(
            &runner,
            Platform::Windows,
            &paths(source, root.clone(), &scratch),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("ntfy-auth.exe is missing"));
        assert!(!root.exists());
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn an_identical_install_keeps_the_files_and_restarts_the_relay() {
        let scratch = Scratch::new("identical");
        let (source, root) = release(&scratch, b"one");
        let runner = windows_runner();
        install(
            &runner,
            Platform::Windows,
            &paths(source.clone(), root.clone(), &scratch),
            false,
        )
        .unwrap();
        let runner = windows_runner();
        runner.set_stdout(
            "reg",
            &format!("    NTFY_RELAY_HOME    REG_SZ    {}\n", root.display()),
        );
        let report = install(
            &runner,
            Platform::Windows,
            &paths(source, root, &scratch),
            false,
        )
        .unwrap();
        assert!(report
            .lines
            .contains(&"install: kept ntfy-relay.exe".to_string()));
        assert!(report.lines.contains(&"result: installed".to_string()));
        assert!(runner.calls().iter().all(|(program, _)| program != "setx"));
        let calls = runner.calls();
        assert!(calls
            .iter()
            .any(|(program, args)| program.ends_with("ntfy-admin.exe") && args[0] == "stop"));
        assert!(calls
            .iter()
            .any(|(program, args)| program.ends_with("ntfy-relay.exe") && args[0] == "ensure"));
    }

    #[test]
    fn a_changed_relay_is_kept_as_previous_and_restarted() {
        let scratch = Scratch::new("changed");
        let (source, root) = release(&scratch, b"one");
        let runner = windows_runner();
        install(
            &runner,
            Platform::Windows,
            &paths(source.clone(), root.clone(), &scratch),
            false,
        )
        .unwrap();
        for stem in BINARIES {
            fs::write(source.join(format!("{stem}.exe")), b"two").unwrap();
        }
        let runner = windows_runner();
        let report = install(
            &runner,
            Platform::Windows,
            &paths(source, root.clone(), &scratch),
            false,
        )
        .unwrap();
        assert_eq!(fs::read(root.join("ntfy-relay.exe")).unwrap(), b"two");
        assert_eq!(
            fs::read(root.join("ntfy-relay.previous.exe")).unwrap(),
            b"one"
        );
        assert!(report
            .lines
            .iter()
            .any(|line| line == "relay: running pid=42"));
        let calls = runner.calls();
        assert!(calls
            .iter()
            .any(|(program, args)| program.ends_with("ntfy-admin.exe") && args[0] == "stop"));
        assert!(calls
            .iter()
            .any(|(program, args)| program.ends_with("ntfy-relay.exe") && args[0] == "ensure"));
    }

    #[test]
    fn a_failed_copy_restores_the_outgoing_image() {
        let scratch = Scratch::new("restore");
        let source = scratch.path().join("source");
        let target = scratch.path().join("target");
        let previous = scratch.path().join("previous");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("ntfy-relay.exe"), b"new").unwrap();
        fs::write(&target, b"old").unwrap();
        fs::write(&previous, b"older").unwrap();
        fs::remove_dir_all(&source).unwrap();
        assert!(place(&source.join("ntfy-relay.exe"), &target, &previous).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
    }

    #[test]
    fn missing_configs_are_written_and_existing_ones_kept() {
        let scratch = Scratch::new("configs");
        let (source, root) = release(&scratch, b"one");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(RELAY_CONFIG), b"{\"keep\":true}\n").unwrap();
        let runner = windows_runner();
        let report = install(
            &runner,
            Platform::Windows,
            &paths(source, root.clone(), &scratch),
            false,
        )
        .unwrap();
        assert_eq!(
            fs::read(root.join(RELAY_CONFIG)).unwrap(),
            b"{\"keep\":true}\n"
        );
        assert!(report
            .lines
            .contains(&"config: kept ntfy-relay.json".to_string()));
        let auth = fs::read_to_string(root.join(AUTH_CONFIG)).unwrap();
        assert!(report
            .lines
            .contains(&"config: written ntfy-auth.json".to_string()));
        let value: serde_json::Value = serde_json::from_str(&auth).unwrap();
        assert_eq!(value["user"], "phone");
        assert_eq!(value["topic"], "*");
        assert_eq!(value["permission"], "rw");
    }

    #[test]
    fn the_default_relay_config_parses_and_serves_no_topics() {
        let value: serde_json::Value = serde_json::from_str(RELAY_CONFIG_TEXT).unwrap();
        assert_eq!(value["ntfy"]["server"], "http://127.0.0.1:8810");
        assert_eq!(value["projectRoots"], serde_json::json!([]));
        assert_eq!(value["outbound"]["enabled"], true);
    }

    #[test]
    fn the_watchdog_task_names_ensure_and_repeats_every_five_minutes() {
        let root = PathBuf::from(r"C:\Users\user\AppData\Local\ntfy-relay");
        let xml = task_xml_watchdog(&root, "S-1-5-21-1");
        assert!(xml.contains("<URI>\\ntfy-relay-watchdog</URI>"));
        assert!(xml.contains("<Interval>PT5M</Interval>"));
        assert!(xml.contains(&format!(
            "<Command>{}</Command>",
            root.join("ntfy-relay.exe").display()
        )));
        assert!(xml.contains("<Arguments>ensure</Arguments>"));
        assert!(xml.contains("<UserId>S-1-5-21-1</UserId>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
    }

    #[test]
    fn the_startup_task_runs_the_hidden_script() {
        let root = PathBuf::from(r"C:\relay");
        let wscript = PathBuf::from(r"C:\Windows\System32\wscript.exe");
        let xml = task_xml_startup(&root, "S-1-5-21-1", &wscript);
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<UserId>S-1-5-21-1</UserId></LogonTrigger>"));
        assert!(xml.contains("<Command>C:\\Windows\\System32\\wscript.exe</Command>"));
        let script = root.join(STARTUP_SCRIPT);
        assert!(xml.contains(&format!("//B //NoLogo &quot;{}&quot;", script.display())));
        assert!(!xml.contains("<Arguments>machine start</Arguments>"));
    }

    #[test]
    fn the_startup_script_runs_the_chain_hidden_and_in_order() {
        let root = PathBuf::from(r"C:\relay");
        let podman = PathBuf::from(r"C:\Program Files\Podman\podman.exe");
        let text = startup_script_text(&root, &podman);
        let machine = text.find("machine start").unwrap();
        let ensure = text.find(" ensure").unwrap();
        assert!(machine < ensure);
        assert!(!text.contains(" apply"));
        assert!(text.contains("\"\"\"C:\\Program Files\\Podman\\podman.exe\"\" machine start\""));
        let relay = root.join("ntfy-relay.exe");
        assert!(text.contains(&format!("\"\"\"{}\"\" ensure\"", relay.display())));
        assert_eq!(text.matches(", 0, True").count(), 2);
        assert!(text.contains("WScript.Quit code"));
    }

    #[test]
    fn a_path_with_xml_characters_is_escaped() {
        let root = PathBuf::from("/srv/a&b/<relay>");
        let xml = task_xml_watchdog(&root, "S-1-5-21-1");
        let escaped = root
            .join("ntfy-relay.exe")
            .to_string_lossy()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        assert!(xml.contains(&escaped), "{xml}");
    }

    #[test]
    fn a_stuck_stop_is_force_killed_from_the_endpoint_pid() {
        let scratch = Scratch::new("stuck");
        let (source, root) = release(&scratch, b"one");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("ntfy-relay-endpoint.json"),
            "{\"pid\":4242,\"port\":1,\"token\":\"x\"}\n",
        )
        .unwrap();
        let runner = windows_runner();
        runner.fail_on("stop", 1);
        runner.set_stdout(
            "powershell",
            &format!("{}\n", root.join("ntfy-relay.exe").display()),
        );
        let report = install(
            &runner,
            Platform::Windows,
            &paths(source, root, &scratch),
            false,
        )
        .unwrap();
        assert!(report.lines.contains(&"result: installed".to_string()));
        let calls = runner.calls();
        let kill = calls
            .iter()
            .find(|(program, _)| program == "taskkill")
            .expect("taskkill");
        assert_eq!(
            kill.1,
            vec!["/F".to_string(), "/PID".to_string(), "4242".to_string()]
        );
    }

    #[test]
    fn a_stale_endpoint_pid_is_not_killed() {
        let scratch = Scratch::new("stale");
        let (source, root) = release(&scratch, b"one");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("ntfy-relay-endpoint.json"),
            "{\"pid\":4242,\"port\":1,\"token\":\"x\"}\n",
        )
        .unwrap();
        let runner = windows_runner();
        runner.fail_on("stop", 3);
        let report = install(
            &runner,
            Platform::Windows,
            &paths(source, root, &scratch),
            false,
        )
        .unwrap();
        assert!(report.lines.contains(&"result: installed".to_string()));
        assert!(
            !runner
                .calls()
                .iter()
                .any(|(program, _)| program == "taskkill"),
            "no relay answered, so the pid cannot be trusted"
        );
    }

    #[test]
    fn a_stuck_stop_with_a_foreign_pid_is_not_killed() {
        let scratch = Scratch::new("foreign");
        let (source, root) = release(&scratch, b"one");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("ntfy-relay-endpoint.json"),
            "{\"pid\":4242,\"port\":1,\"token\":\"x\"}\n",
        )
        .unwrap();
        let runner = windows_runner();
        runner.fail_on("stop", 1);
        runner.set_stdout("powershell", "C:\\Windows\\System32\\notepad.exe\n");
        let report = install(
            &runner,
            Platform::Windows,
            &paths(source, root, &scratch),
            false,
        )
        .unwrap();
        assert!(report
            .lines
            .contains(&"stop: not killed, endpoint stale".to_string()));
        assert!(
            !runner
                .calls()
                .iter()
                .any(|(program, _)| program == "taskkill"),
            "a pid that does not run the root relay must not be killed"
        );
    }

    #[test]
    fn a_failed_copy_leaves_the_old_image_in_place() {
        let scratch = Scratch::new("place-fail");
        let dir = scratch.path();
        let target = dir.join("ntfy-relay.exe");
        fs::write(&target, b"old").unwrap();
        let previous = dir.join("ntfy-relay.previous.exe");
        let source = dir.join("source-dir");
        fs::create_dir_all(&source).unwrap();
        let result = place(&source, &target, &previous);
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(!staged_path(&target).exists());
    }

    #[test]
    fn remove_deletes_a_stale_staged_image() {
        let scratch = Scratch::new("staged-remove");
        let (source, root) = release(&scratch, b"one");
        fs::create_dir_all(&root).unwrap();
        let staged = root.join("ntfy-relay.exe.new");
        fs::write(&staged, b"partial").unwrap();
        let runner = windows_runner();
        let report = remove(
            &runner,
            Platform::Windows,
            &paths(source, root, &scratch),
            false,
        )
        .unwrap();
        assert!(!staged.exists());
        assert!(report
            .lines
            .contains(&"remove: deleted ntfy-relay.exe.new".to_string()));
    }

    #[test]
    fn the_endpoint_pid_is_read_from_the_runtime_file() {
        let scratch = Scratch::new("endpoint");
        let root = scratch.path();
        fs::write(
            root.join("ntfy-relay-endpoint.json"),
            "{\"pid\":77,\"port\":5000,\"token\":\"t\"}\n",
        )
        .unwrap();
        assert_eq!(endpoint_pid(root), Some(77));
        fs::write(root.join("ntfy-relay-endpoint.json"), "not json").unwrap();
        assert_eq!(endpoint_pid(root), None);
    }

    #[test]
    fn the_task_file_is_utf16le_with_a_bom() {
        let bytes = encode_task("A\u{e9}");
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        assert_eq!(&bytes[2..], &[0x41, 0x00, 0xE9, 0x00]);
    }

    #[test]
    fn the_linux_units_name_the_root_and_the_commands() {
        let root = PathBuf::from("/home/user/.local/share/ntfy-relay");
        let service = unit_watchdog_service(&root);
        assert!(service.contains("WorkingDirectory=/home/user/.local/share/ntfy-relay"));
        assert!(
            service.contains("ExecStart=\"/home/user/.local/share/ntfy-relay/ntfy-relay\" ensure")
        );
        let timer = unit_watchdog_timer();
        assert!(timer.contains("OnBootSec=1min"));
        assert!(timer.contains("OnUnitActiveSec=5min"));
        assert!(timer.contains("WantedBy=timers.target"));
        let startup = unit_startup_service(&root);
        assert!(!startup.contains("apply"));
        assert!(startup.contains("\"/home/user/.local/share/ntfy-relay/ntfy-relay\" ensure"));
        assert!(startup.contains("WantedBy=default.target"));
    }

    #[test]
    fn a_whoami_row_yields_the_sid() {
        let text = "\"DESKTOP-1\\user\",\"S-1-5-21-2929337068-1001\"\n";
        assert_eq!(parse_sid(text).as_deref(), Some("S-1-5-21-2929337068-1001"));
        assert_eq!(parse_sid("nonsense"), None);
    }

    #[test]
    fn where_output_yields_the_first_path() {
        let text = "C:\\Program Files\\Podman\\podman.exe\r\nC:\\other\\podman.exe\r\n";
        assert_eq!(
            parse_where(text),
            Some(PathBuf::from("C:\\Program Files\\Podman\\podman.exe"))
        );
        assert_eq!(parse_where(""), None);
    }

    #[test]
    fn an_existing_variable_is_kept() {
        let scratch = Scratch::new("env");
        let (source, root) = release(&scratch, b"one");
        let runner = windows_runner();
        runner.set_stdout(
            "reg",
            &format!("    NTFY_RELAY_HOME    REG_SZ    {}\n", root.display()),
        );
        let report = install(
            &runner,
            Platform::Windows,
            &paths(source, root, &scratch),
            false,
        )
        .unwrap();
        assert!(report
            .lines
            .contains(&"env: kept NTFY_RELAY_HOME".to_string()));
        assert!(runner.calls().iter().all(|(program, _)| program != "setx"));
    }

    #[test]
    fn a_sibling_prefix_is_not_the_configured_variable() {
        let scratch = Scratch::new("env-prefix");
        let (source, root) = release(&scratch, b"one");
        let runner = windows_runner();
        runner.set_stdout(
            "reg",
            &format!("    NTFY_RELAY_HOME    REG_SZ    {}-old\n", root.display()),
        );
        let report = install(
            &runner,
            Platform::Windows,
            &paths(source, root.clone(), &scratch),
            false,
        )
        .unwrap();
        assert!(report
            .lines
            .contains(&"env: set NTFY_RELAY_HOME".to_string()));
        let calls = runner.calls();
        let setx = calls.iter().find(|(program, _)| program == "setx").unwrap();
        assert_eq!(setx.1[1], root.display().to_string());
    }

    #[test]
    fn the_registry_value_is_read_exactly() {
        let text = "\r\nHKEY_CURRENT_USER\\Environment\r\n    NTFY_RELAY_HOME    REG_EXPAND_SZ    C:\\A B\\relay;C:\\other\r\n\r\n";
        assert_eq!(
            registry_value(text, "NTFY_RELAY_HOME"),
            Some("C:\\A B\\relay;C:\\other")
        );
        assert_eq!(registry_value(text, "OTHER"), None);
        assert_eq!(
            registry_value("    NTFY_RELAY_HOME    REG_SZ\r\n", "NTFY_RELAY_HOME"),
            Some("")
        );
    }

    #[test]
    fn a_missing_variable_is_set_once() {
        let scratch = Scratch::new("setenv");
        let (source, root) = release(&scratch, b"one");
        let expected = root.display().to_string();
        let runner = windows_runner();
        runner.fail_on("reg", 1);
        let report = install(
            &runner,
            Platform::Windows,
            &paths(source, root, &scratch),
            false,
        )
        .unwrap();
        assert!(report
            .lines
            .contains(&"env: set NTFY_RELAY_HOME".to_string()));
        let calls = runner.calls();
        let setx = calls.iter().find(|(program, _)| program == "setx").unwrap();
        assert_eq!(setx.1[0], "NTFY_RELAY_HOME");
        assert_eq!(setx.1[1], expected);
    }

    #[test]
    fn the_legacy_shortcut_is_removed_when_present() {
        let scratch = Scratch::new("shortcut");
        let (source, root) = release(&scratch, b"one");
        let paths = paths(source, root, &scratch);
        let shortcut = paths.shortcut().unwrap();
        fs::create_dir_all(shortcut.parent().unwrap()).unwrap();
        fs::write(&shortcut, b"lnk").unwrap();
        let runner = windows_runner();
        let report = install(&runner, Platform::Windows, &paths, false).unwrap();
        assert!(!shortcut.exists());
        assert!(report.lines.contains(&"shortcut: removed".to_string()));
    }

    #[test]
    fn a_dry_run_changes_nothing() {
        let scratch = Scratch::new("dry");
        let (source, root) = release(&scratch, b"one");
        let paths = paths(source, root.clone(), &scratch);
        let runner = windows_runner();
        runner.fail_on("reg", 1);
        let report = install(&runner, Platform::Windows, &paths, true).unwrap();
        assert!(!root.exists());
        assert!(report
            .lines
            .iter()
            .any(|line| line.starts_with("install: would place")));
        assert!(report
            .lines
            .contains(&format!("script: would write {STARTUP_SCRIPT}")));
        assert!(report.lines.contains(&"result: planned".to_string()));
        assert!(runner.calls().iter().all(|(program, _)| program != "setx"));
        assert!(runner
            .calls()
            .iter()
            .all(|(program, _)| program != "schtasks"));
    }

    #[test]
    fn install_registers_both_tasks_and_sets_the_variable() {
        let scratch = Scratch::new("tasks");
        let (source, root) = release(&scratch, b"one");
        let runner = windows_runner();
        runner.fail_on("reg", 1);
        let report = install(
            &runner,
            Platform::Windows,
            &paths(source, root, &scratch),
            false,
        )
        .unwrap();
        assert!(report
            .lines
            .contains(&"task: registered ntfy-relay-watchdog".to_string()));
        assert!(report
            .lines
            .contains(&"task: registered ntfy-relay-startup".to_string()));
        let calls = runner.calls();
        let creates = calls
            .iter()
            .filter(|(program, args)| program == "schtasks" && args[0] == "/Create")
            .count();
        assert_eq!(creates, 2);
    }

    #[test]
    fn install_writes_the_hidden_startup_script() {
        let scratch = Scratch::new("startup-script");
        let (source, root) = release(&scratch, b"one");
        let runner = windows_runner();
        runner.fail_on("reg", 1);
        let report = install(
            &runner,
            Platform::Windows,
            &paths(source, root.clone(), &scratch),
            false,
        )
        .unwrap();
        let text = fs::read_to_string(root.join(STARTUP_SCRIPT)).unwrap();
        assert!(text.contains("machine start"));
        assert!(!text.contains(" apply"));
        assert!(text.contains("ntfy-relay.exe\"\" ensure"));
        assert!(report
            .lines
            .contains(&format!("script: written {STARTUP_SCRIPT}")));
    }

    #[test]
    fn the_installed_scripts_folder_is_removed() {
        let scratch = Scratch::new("scripts");
        let (source, root) = release(&scratch, b"one");
        let scripts = root.join("relay-windows-scripts");
        fs::create_dir_all(&scripts).unwrap();
        fs::write(scripts.join("startup-check.ps1"), b"x").unwrap();
        let runner = windows_runner();
        let report = install(
            &runner,
            Platform::Windows,
            &paths(source, root.clone(), &scratch),
            false,
        )
        .unwrap();
        assert!(!scripts.exists());
        assert!(report
            .lines
            .contains(&"scripts: removed relay-windows-scripts".to_string()));
    }

    #[test]
    fn a_previous_name_held_by_a_running_image_moves_to_a_numbered_copy() {
        let scratch = Scratch::new("previous-locked");
        let (source, root) = release(&scratch, b"one");
        let runner = windows_runner();
        install(
            &runner,
            Platform::Windows,
            &paths(source.clone(), root.clone(), &scratch),
            false,
        )
        .unwrap();
        // A directory stands in for the file a running MCP process keeps open:
        // it cannot be removed, which is what the installer has to survive.
        let canonical = root.join("ntfy-relay.previous.exe");
        fs::create_dir_all(&canonical).unwrap();
        for stem in BINARIES {
            fs::write(source.join(format!("{stem}.exe")), b"two").unwrap();
        }
        let runner = windows_runner();
        let report = install(
            &runner,
            Platform::Windows,
            &paths(source, root.clone(), &scratch),
            false,
        )
        .unwrap();
        assert_eq!(fs::read(root.join("ntfy-relay.exe")).unwrap(), b"two");
        assert_eq!(
            fs::read(root.join("ntfy-relay.previous.2.exe")).unwrap(),
            b"one"
        );
        assert!(canonical.is_dir());
        assert!(
            report
                .lines
                .iter()
                .any(|line| line
                    == "install: placed ntfy-relay.exe, previous ntfy-relay.previous.2.exe"),
            "{:#?}",
            report.lines
        );
        let _ = fs::remove_dir_all(canonical);
    }

    #[test]
    fn remove_reports_a_previous_copy_it_cannot_delete() {
        let scratch = Scratch::new("remove-locked");
        let (source, root) = release(&scratch, b"one");
        let runner = windows_runner();
        install(
            &runner,
            Platform::Windows,
            &paths(source.clone(), root.clone(), &scratch),
            false,
        )
        .unwrap();
        let canonical = root.join("ntfy-relay.previous.exe");
        fs::create_dir_all(&canonical).unwrap();
        let runner = windows_runner();
        let report = remove(
            &runner,
            Platform::Windows,
            &paths(source, root.clone(), &scratch),
            false,
        )
        .unwrap();
        assert!(canonical.is_dir());
        assert!(
            report
                .lines
                .iter()
                .any(|line| line.starts_with("remove: kept ntfy-relay.previous.exe")),
            "{:#?}",
            report.lines
        );
        assert!(report.lines.contains(&"result: removed".to_string()));
        let _ = fs::remove_dir_all(canonical);
    }

    #[test]
    fn remove_deletes_the_images_and_keeps_the_data() {
        let scratch = Scratch::new("remove");
        let (source, root) = release(&scratch, b"one");
        let runner = windows_runner();
        install(
            &runner,
            Platform::Windows,
            &paths(source.clone(), root.clone(), &scratch),
            false,
        )
        .unwrap();
        fs::write(root.join("ntfy-token.txt"), b"token").unwrap();
        fs::write(root.join("ntfy-relay-state.json"), b"{}").unwrap();
        let runner = windows_runner();
        let report = remove(
            &runner,
            Platform::Windows,
            &paths(source, root.clone(), &scratch),
            false,
        )
        .unwrap();
        for stem in BINARIES {
            assert!(!root.join(format!("{stem}.exe")).exists(), "{stem}");
        }
        assert!(!root.join(STARTUP_SCRIPT).exists());
        assert!(root.join("ntfy-token.txt").exists());
        assert!(root.join("ntfy-relay.json").exists());
        assert!(report.lines.contains(&"result: removed".to_string()));
        assert!(report
            .lines
            .contains(&"kept: configs, token, state, asks, journal, log".to_string()));
    }

    #[test]
    fn a_remove_dry_run_stops_nothing() {
        let scratch = Scratch::new("remove-dry");
        let (source, root) = release(&scratch, b"one");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(STARTUP_SCRIPT), b"x").unwrap();
        let runner = windows_runner();
        let report = remove(
            &runner,
            Platform::Windows,
            &paths(source, root, &scratch),
            true,
        )
        .unwrap();
        assert!(report.lines.contains(&"relay: would stop".to_string()));
        assert!(report
            .lines
            .contains(&format!("script: would delete {STARTUP_SCRIPT}")));
        assert!(report.lines.contains(&"result: planned".to_string()));
        assert!(runner
            .calls()
            .iter()
            .all(|(program, _)| !program.ends_with("ntfy-admin.exe")));
    }

    #[test]
    fn remove_is_idempotent_on_a_half_installed_root() {
        let scratch = Scratch::new("remove-twice");
        let source = scratch.path().join("release");
        let root = scratch.path().join("root");
        fs::create_dir_all(&source).unwrap();
        for stem in BINARIES {
            fs::write(source.join(format!("{stem}.exe")), b"one").unwrap();
        }
        let runner = windows_runner();
        runner.fail_on("reg", 1);
        runner.fail_on("stop", 3);
        let report = remove(
            &runner,
            Platform::Windows,
            &paths(source, root, &scratch),
            false,
        )
        .unwrap();
        assert!(report.lines.contains(&"result: removed".to_string()));
        assert!(report
            .lines
            .contains(&"env: absent NTFY_RELAY_HOME".to_string()));
    }

    #[test]
    fn a_linux_install_writes_units_and_enables_them() {
        let scratch = Scratch::new("linux");
        let source = scratch.path().join("release");
        let root = scratch.path().join("root");
        let unit_dir = scratch.path().join("units");
        fs::create_dir_all(&source).unwrap();
        for stem in BINARIES {
            fs::write(source.join(stem), b"one").unwrap();
        }
        let runner = FakeRunner::new();
        runner.set_stdout("ensure", "started pid=7\n");
        let paths = InstallPaths {
            root: root.clone(),
            source,
            app_data: None,
            unit_dir: Some(unit_dir.clone()),
            user_base: None,
        };
        let report = install(&runner, Platform::Linux, &paths, false).unwrap();
        assert!(unit_dir.join("ntfy-relay-watchdog.service").exists());
        assert!(unit_dir.join("ntfy-relay-watchdog.timer").exists());
        assert!(unit_dir.join("ntfy-relay-startup.service").exists());
        assert!(report
            .lines
            .contains(&"unit: written ntfy-relay-watchdog.timer".to_string()));
        let calls = runner.calls();
        assert!(calls.iter().any(|(program, args)| {
            program == "systemctl" && args.contains(&"enable".to_string())
        }));
    }

    #[test]
    fn a_linux_install_without_systemd_reports_the_missing_autostart() {
        let scratch = Scratch::new("linux-no-systemd");
        let source = scratch.path().join("release");
        let root = scratch.path().join("root");
        let unit_dir = scratch.path().join("units");
        fs::create_dir_all(&source).unwrap();
        for stem in BINARIES {
            fs::write(source.join(stem), b"one").unwrap();
        }
        let runner = FakeRunner::new();
        runner.set_stdout("ensure", "started pid=7\n");
        runner.without_program("systemctl");
        let paths = InstallPaths {
            root,
            source,
            app_data: None,
            unit_dir: Some(unit_dir.clone()),
            user_base: None,
        };
        let report = install(&runner, Platform::Linux, &paths, false).unwrap();
        assert!(!unit_dir.exists());
        assert!(report
            .lines
            .contains(&"unit: skipped, no systemd (systemctl not found)".to_string()));
        assert!(report
            .lines
            .iter()
            .any(|line| line.starts_with("warning: no autostart")));
        assert!(report
            .lines
            .contains(&"result: installed, without autostart".to_string()));
        assert!(report.lines.contains(&"relay: started pid=7".to_string()));
    }

    #[test]
    fn a_linux_install_without_a_user_session_reports_the_missing_autostart() {
        let scratch = Scratch::new("linux-no-user-session");
        let source = scratch.path().join("release");
        let root = scratch.path().join("root");
        let unit_dir = scratch.path().join("units");
        fs::create_dir_all(&source).unwrap();
        for stem in BINARIES {
            fs::write(source.join(stem), b"one").unwrap();
        }
        let runner = FakeRunner::new();
        runner.set_stdout("ensure", "started pid=7\n");
        runner.fail_on_call("systemctl", &["--user", "show-environment"], 1);
        let paths = InstallPaths {
            root,
            source,
            app_data: None,
            unit_dir: Some(unit_dir.clone()),
            user_base: None,
        };
        let report = install(&runner, Platform::Linux, &paths, false).unwrap();
        assert!(!unit_dir.exists());
        assert!(report.lines.contains(
            &"unit: skipped, no systemd user session (systemctl --user show-environment failed)"
                .to_string()
        ));
        assert!(report
            .lines
            .iter()
            .any(|line| line.starts_with("warning: no autostart")));
        assert!(report
            .lines
            .contains(&"result: installed, without autostart".to_string()));
        assert!(report.lines.contains(&"relay: started pid=7".to_string()));
        assert!(!runner.calls().iter().any(|(program, args)| {
            program == "systemctl" && args.contains(&"enable".to_string())
        }));
    }

    #[test]
    fn a_linux_remove_without_a_user_session_deletes_units_without_disable_calls() {
        let scratch = Scratch::new("linux-remove-no-user-session");
        let source = scratch.path().join("release");
        let root = scratch.path().join("root");
        let unit_dir = scratch.path().join("units");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&unit_dir).unwrap();
        for stem in BINARIES {
            fs::write(source.join(stem), b"one").unwrap();
        }
        fs::write(unit_dir.join("ntfy-relay-watchdog.timer"), b"unit").unwrap();
        let runner = FakeRunner::new();
        runner.fail_on_call("systemctl", &["--user", "show-environment"], 1);
        let paths = InstallPaths {
            root,
            source,
            app_data: None,
            unit_dir: Some(unit_dir.clone()),
            user_base: None,
        };
        let report = remove(&runner, Platform::Linux, &paths, false).unwrap();
        assert!(!unit_dir.join("ntfy-relay-watchdog.timer").exists());
        assert!(report.lines.contains(
            &"unit: skipped, no systemd user session (systemctl --user show-environment failed)"
                .to_string()
        ));
        assert!(report.lines.contains(&"result: removed".to_string()));
        assert!(!runner.calls().iter().any(|(program, args)| {
            program == "systemctl"
                && (args.contains(&"disable".to_string())
                    || args.contains(&"daemon-reload".to_string()))
        }));
    }

    #[test]
    fn a_linux_remove_without_systemd_deletes_stale_units_without_systemctl() {
        let scratch = Scratch::new("linux-remove-no-systemd");
        let source = scratch.path().join("release");
        let root = scratch.path().join("root");
        let unit_dir = scratch.path().join("units");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&unit_dir).unwrap();
        for stem in BINARIES {
            fs::write(source.join(stem), b"one").unwrap();
        }
        fs::write(unit_dir.join("ntfy-relay-watchdog.timer"), b"unit").unwrap();
        let runner = FakeRunner::new();
        runner.without_program("systemctl");
        let paths = InstallPaths {
            root,
            source,
            app_data: None,
            unit_dir: Some(unit_dir.clone()),
            user_base: None,
        };
        let report = remove(&runner, Platform::Linux, &paths, false).unwrap();
        assert!(!unit_dir.join("ntfy-relay-watchdog.timer").exists());
        assert!(report
            .lines
            .contains(&"unit: skipped, no systemd (systemctl not found)".to_string()));
        assert!(report.lines.contains(&"result: removed".to_string()));
        assert!(runner
            .calls()
            .iter()
            .filter(|(program, _)| program == "systemctl")
            .all(|(_, args)| *args == ["--version".to_string()]));
    }
}
