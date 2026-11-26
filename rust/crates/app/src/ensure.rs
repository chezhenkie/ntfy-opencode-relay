//! `ensure`: the one-shot watchdog step.
//!
//! Exits 0 when the relay is already running or is disabled by config. Otherwise
//! it starts the daemon detached from this process, redirects its output to the
//! install-root logs, and waits for the lock. The scheduled task names this
//! subcommand, so the start wrapper is not in its path.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ntfy_relay_core::config::{install_root, load_config};

use crate::control_endpoint;
use crate::lock::{live_lock, lock_owner, lock_path_for, PHASE_READY};
use crate::logger::format_timestamp;
use crate::util::now_ms;

/// A cold start takes well under a second. A minute leaves room for a slow
/// antivirus scan or a busy config while keeping the watchdog call short.
const START_TIMEOUT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(250);
const LOG_LIMIT: u64 = 1024 * 1024;

pub fn ensure(config_path: &Path) -> Result<(), String> {
    let config = load_config(config_path).map_err(|error| error.to_string())?;
    let lock_file = lock_path_for(&config.state_file);
    let root = install_root();

    if !config.enabled {
        let live = live_lock(&lock_file)
            .map_err(|error| format!("lock {}: {error}", lock_file.display()))?;
        if let (Some(body), Some(root)) = (live, root.as_deref()) {
            if let Err(error) = control_endpoint::request_stop(root) {
                append_line(
                    &root.join("logs").join("relay-ensure.log"),
                    &format!("FAIL stop pid={} {error}", body.pid),
                );
                println!("disabled, stop failed: {error}");
                return Ok(());
            }
        }
        println!("disabled");
        return Ok(());
    }

    if let Some(body) =
        live_lock(&lock_file).map_err(|error| format!("lock {}: {error}", lock_file.display()))?
    {
        println!("{}", live_line(&body));
        return Ok(());
    }

    let root = root.ok_or_else(|| "cannot determine the install root".to_string())?;
    let logs = root.join("logs");
    fs::create_dir_all(&logs).map_err(|error| format!("logs {}: {error}", logs.display()))?;
    let ensure_log = logs.join("relay-ensure.log");
    append_line(&ensure_log, "relay down, starting");

    let mut child = spawn(&root, &logs).map_err(|error| {
        append_line(&ensure_log, &format!("FAIL relay-start {error}"));
        error
    })?;

    match wait_for_start(&lock_file, &mut child, START_TIMEOUT) {
        Ok(pid) => {
            println!("started pid={pid}");
            Ok(())
        }
        Err(detail) => {
            let error = format!(
                "relay not running: {detail}, see {}",
                logs.join("relay-stderr.log").display()
            );
            append_line(&ensure_log, &format!("FAIL relay-start {error}"));
            Err(error)
        }
    }
}

/// The one flush point for a start or a failure. The watcher stays silent while
/// the relay is alive, so a healthy five-minute tick writes nothing.
fn append_line(file: &Path, text: &str) {
    let _ = write_line(file, text);
}

fn live_line(body: &crate::lock::LockBody) -> String {
    if body.phase == PHASE_READY {
        format!("running pid={}", body.pid)
    } else {
        format!("starting pid={}", body.pid)
    }
}

fn write_line(file: &Path, text: &str) -> io::Result<()> {
    write_line_limited(file, text, LOG_LIMIT)
}

fn write_line_limited(file: &Path, text: &str, limit: u64) -> io::Result<()> {
    if matches!(fs::metadata(file), Ok(meta) if meta.len() >= limit) {
        let rotated = file.with_extension("log.1");
        let _ = fs::remove_file(&rotated);
        let _ = fs::rename(file, &rotated);
    }
    let mut handle = OpenOptions::new().create(true).append(true).open(file)?;
    writeln!(handle, "{} {text}", format_timestamp(now_ms()))
}

fn spawn(root: &Path, logs: &Path) -> Result<Child, String> {
    let exe = std::env::current_exe().map_err(|error| format!("current exe: {error}"))?;
    let stdout = open_log(&logs.join("relay-stdout.log"))?;
    let stderr = open_log(&logs.join("relay-stderr.log"))?;
    let mut command = Command::new(&exe);
    command
        .arg("run")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A new process group keeps the daemon off the terminal's SIGHUP path.
        command.process_group(0);
    }
    command
        .spawn()
        .map_err(|error| format!("spawn {}: {error}", exe.display()))
}

fn open_log(path: &Path) -> Result<std::fs::File, String> {
    if matches!(fs::metadata(path), Ok(meta) if meta.len() >= LOG_LIMIT) {
        let rotated = path.with_extension("log.1");
        let _ = fs::remove_file(&rotated);
        let _ = fs::rename(path, &rotated);
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| format!("log {}: {error}", path.display()))
}

/// Polls the lock until it names a live relay image. A child that exits first is
/// reported at once, unless another start won the race and holds the lock. A
/// file caught mid-write reads as corrupt, so a read error is retried until the
/// deadline.
fn wait_for_start(lock: &Path, child: &mut Child, timeout: Duration) -> Result<u32, String> {
    let deadline = Instant::now() + timeout;
    let mut last_error: Option<io::Error>;
    loop {
        match lock_owner(lock) {
            Ok(Some(pid)) => return Ok(pid),
            Ok(None) => last_error = None,
            Err(error) => last_error = Some(error),
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return match lock_owner(lock) {
                    Ok(Some(pid)) => Ok(pid),
                    _ => Err(format!("relay exited with {status}")),
                }
            }
            Ok(None) => {}
            Err(error) => return Err(format!("relay process: {error}")),
        }
        if Instant::now() >= deadline {
            return Err(match last_error {
                Some(error) => format!(
                    "no live lock after {} s, lock unreadable: {error}",
                    timeout.as_secs()
                ),
                None => format!("no live lock after {} s", timeout.as_secs()),
            });
        }
        thread::sleep(POLL.min(timeout));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntfy_relay_core::test_support::Scratch;

    fn temp_dir(name: &str) -> Scratch {
        Scratch::new(&format!("app-ensure-{name}"))
    }

    #[test]
    fn only_a_ready_lock_reports_running() {
        let ready = crate::lock::LockBody {
            pid: 42,
            started: "x".to_string(),
            phase: PHASE_READY.to_string(),
            exe: String::new(),
        };
        assert_eq!(live_line(&ready), "running pid=42");
        let starting = crate::lock::LockBody {
            phase: crate::lock::PHASE_STARTING.to_string(),
            ..ready
        };
        assert_eq!(live_line(&starting), "starting pid=42");
    }

    #[test]
    fn a_full_child_log_rotates_and_starts_empty() {
        let dir = temp_dir("log-rotate");
        let path = dir.join("relay-stdout.log");
        fs::write(&path, vec![b'x'; (LOG_LIMIT + 1) as usize]).unwrap();
        drop(open_log(&path).unwrap());
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(
            fs::metadata(dir.join("relay-stdout.log.1")).unwrap().len(),
            LOG_LIMIT + 1
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_child_log_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("log-mode");
        let path = dir.join("relay-stderr.log");
        drop(open_log(&path).unwrap());
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_live_lock_ends_the_wait_at_once() {
        let dir = temp_dir("live");
        let path = dir.join("relay.lock");
        let pid = std::process::id();
        fs::write(&path, format!(r#"{{"pid":{pid},"started":"x"}}"#)).unwrap();
        let mut child = sleeper();
        let started = Instant::now();
        assert_eq!(
            wait_for_start(&path, &mut child, Duration::from_secs(5)),
            Ok(pid)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        stop(&mut child);
    }

    #[test]
    fn an_exited_child_is_reported_without_the_deadline() {
        let dir = temp_dir("exited");
        let path = dir.join("relay.lock");
        let (program, args) = exit_command();
        let mut child = Command::new(program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        let error = wait_for_start(&path, &mut child, Duration::from_secs(30)).unwrap_err();
        assert!(error.contains("relay exited"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = child.wait();
    }

    #[test]
    fn a_missing_lock_times_out() {
        let dir = temp_dir("missing");
        let path = dir.join("relay.lock");
        let mut child = sleeper();
        let error = wait_for_start(&path, &mut child, POLL).unwrap_err();
        assert!(error.contains("no live lock"), "{error}");
        stop(&mut child);
    }

    #[test]
    fn a_corrupt_lock_reports_the_read_error() {
        let dir = temp_dir("corrupt");
        let path = dir.join("relay.lock");
        fs::write(&path, "not json").unwrap();
        let mut child = sleeper();
        let error = wait_for_start(&path, &mut child, POLL).unwrap_err();
        assert!(error.contains("lock unreadable"), "{error}");
        stop(&mut child);
    }

    #[test]
    fn a_log_rotates_at_the_limit() {
        let dir = temp_dir("rotate");
        let path = dir.join("relay-ensure.log");
        fs::write(&path, "0123456789\n").unwrap();
        write_line_limited(&path, "new", 4).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap().matches("new").count(), 1);
        let old = fs::read_to_string(path.with_extension("log.1")).unwrap();
        assert_eq!(old, "0123456789\n");
    }

    fn stop(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(windows)]
    fn sleeper() -> Child {
        Command::new("cmd.exe")
            .args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    fn sleeper() -> Child {
        Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[cfg(windows)]
    fn exit_command() -> (&'static str, &'static [&'static str]) {
        ("cmd.exe", &["/c", "exit", "7"])
    }

    #[cfg(target_os = "linux")]
    fn exit_command() -> (&'static str, &'static [&'static str]) {
        ("sh", &["-c", "exit 7"])
    }
}
