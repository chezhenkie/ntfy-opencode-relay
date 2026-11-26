//! Single-relay lock.
//!
//! Mirrors `acquireLock` and `releaseLock` in `ntfy-inbox-relay.mjs`: create
//! `wx`, write the pid, and on contention read the existing file. A live pid
//! from the same image wins, a dead or reused pid is replaced once, and an
//! unreadable file fails closed. The body also carries the start phase, so
//! `ensure` and `status` can tell a relay that is waiting for its token from
//! one that is subscribed, and the image path that holds it.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use ntfy_relay_control::process::pid_alive;
use serde_json::{json, Value};

use crate::logger::format_timestamp;

pub const PHASE_STARTING: &str = "starting";
pub const PHASE_READY: &str = "ready";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockBody {
    pub pid: u32,
    pub started: String,
    pub phase: String,
    pub exe: String,
}

impl LockBody {
    fn to_value(&self) -> Value {
        json!({
            "pid": self.pid,
            "started": self.started,
            "phase": self.phase,
            "exe": self.exe,
        })
    }

    /// A lock written before the phase field reads as `starting`, the safe
    /// answer: it never claims a relay is subscribed when it may not be.
    fn from_value(value: &Value) -> Option<Self> {
        let pid = value
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)?;
        Some(Self {
            pid,
            started: value
                .get("started")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            phase: value
                .get("phase")
                .and_then(Value::as_str)
                .unwrap_or(PHASE_STARTING)
                .to_string(),
            exe: value
                .get("exe")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquired {
    Yes,
    Running(u32),
    Unreadable,
    Contended,
}

pub fn lock_path_for(state_file: &Path) -> PathBuf {
    let text = state_file.to_string_lossy();
    let stem = if text.len() >= 5
        && text.is_char_boundary(text.len() - 5)
        && text[text.len() - 5..].eq_ignore_ascii_case(".json")
    {
        &text[..text.len() - 5]
    } else {
        text.as_ref()
    };
    PathBuf::from(format!("{stem}.lock"))
}

pub fn acquire_lock(path: &Path, pid: u32, now_ms: i64) -> io::Result<Acquired> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let body = LockBody {
        pid,
        started: format_timestamp(now_ms),
        phase: PHASE_STARTING.to_string(),
        exe: std::env::current_exe()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
    .to_value()
    .to_string();

    for _attempt in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut handle) => {
                if let Err(error) = handle.write_all(body.as_bytes()) {
                    drop(handle);
                    let _ = fs::remove_file(path);
                    return Err(error);
                }
                return Ok(Acquired::Yes);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }

        let mut missing = false;
        let mut existing = None;
        for read_attempt in 0..3 {
            match fs::read_to_string(path) {
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Ok(value) => {
                        existing = Some(value);
                        break;
                    }
                    Err(_) if read_attempt < 2 => thread::sleep(Duration::from_millis(20)),
                    Err(_) => return Ok(Acquired::Unreadable),
                },
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    missing = true;
                    break;
                }
                Err(_) if read_attempt < 2 => thread::sleep(Duration::from_millis(20)),
                Err(_) => return Ok(Acquired::Unreadable),
            }
        }
        if missing {
            continue;
        }
        let Some(existing) = existing else {
            return Ok(Acquired::Unreadable);
        };
        let Some(existing_body) = LockBody::from_value(&existing) else {
            return Ok(Acquired::Unreadable);
        };
        if existing_body.pid != pid && pid_is_relay(existing_body.pid, &existing_body.exe) {
            return Ok(Acquired::Running(existing_body.pid));
        }
        fs::remove_file(path)?;
    }
    Ok(Acquired::Contended)
}

/// Rewrites the phase of a lock that still names `pid`. A missing lock or a
/// lock owned by another process is left alone; the write is a temp sibling
/// renamed over the file, so a reader never sees half a body.
pub fn set_phase(path: &Path, pid: u32, phase: &str) -> io::Result<()> {
    let Some(mut body) = read_lock_body(path)? else {
        return Ok(());
    };
    if body.pid != pid || body.phase == phase {
        return Ok(());
    }
    body.phase = phase.to_string();
    write_lock_body(path, &body)
}

fn write_lock_body(path: &Path, body: &LockBody) -> io::Result<()> {
    let temp = path.with_extension("lock.tmp");
    let text = body.to_value().to_string();
    let mut handle = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp)?;
    if let Err(error) = handle.write_all(text.as_bytes()) {
        drop(handle);
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    handle.sync_all()?;
    drop(handle);
    fs::rename(&temp, path)
}

pub fn release_lock(path: &Path, pid: u32) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let Ok(existing) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    if existing.get("pid").and_then(Value::as_u64) == Some(u64::from(pid)) {
        let _ = fs::remove_file(path);
    }
}

pub fn read_lock_body(path: &Path) -> io::Result<Option<LockBody>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(LockBody::from_value(&value))
}

/// The lock body when its pid names a live relay image. A dead pid, a pid
/// reused by another program, or a missing lock reads as `None`; a corrupt
/// file is an error, because a second relay would fail closed on it too.
pub fn live_lock(path: &Path) -> io::Result<Option<LockBody>> {
    match read_lock_body(path)? {
        Some(body) if pid_is_relay(body.pid, &body.exe) => Ok(Some(body)),
        _ => Ok(None),
    }
}

/// The lock pid when it names a live relay image.
pub fn lock_owner(path: &Path) -> io::Result<Option<u32>> {
    Ok(live_lock(path)?.map(|body| body.pid))
}

impl fmt::Display for Acquired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Yes => f.write_str("acquired"),
            Self::Running(pid) => write!(f, "alive pid {pid}"),
            Self::Unreadable => f.write_str("unreadable"),
            Self::Contended => f.write_str("contended"),
        }
    }
}

/// A live pid only blocks the lock when it runs this relay image. Windows can
/// reuse a pid right after a hard kill; without the image check the lock looks
/// live and the next start exits until that unrelated process ends. The full
/// path from the lock body separates two installs; a lock written before the
/// `exe` field existed falls back to the image file name.
fn pid_is_relay(pid: u32, expected_exe: &str) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    same_image(pid, expected_exe).unwrap_or(true)
}

/// `None` means the image could not be read; the caller must stay conservative.
#[cfg(windows)]
fn same_image(pid: u32, expected_exe: &str) -> Option<bool> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut buffer = [0u16; 1024];
    let mut size = buffer.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) };
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        return None;
    }
    let target = String::from_utf16_lossy(&buffer[..size as usize]);
    let current = std::env::current_exe().ok()?;
    Some(image_matches(
        &target,
        &current.to_string_lossy(),
        expected_exe,
    ))
}

#[cfg(windows)]
fn image_matches(target: &str, current: &str, expected: &str) -> bool {
    if expected.is_empty() {
        return image_name(target) == image_name(current);
    }
    target.to_lowercase() == expected.to_lowercase()
}

#[cfg(windows)]
fn image_name(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
}

#[cfg(target_os = "linux")]
fn same_image(pid: u32, expected_exe: &str) -> Option<bool> {
    let target = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let current = std::env::current_exe().ok()?;
    if expected_exe.is_empty() {
        return Some(target.file_name()? == current.file_name()?);
    }
    // A binary replaced in place keeps running from a deleted inode whose
    // link carries this suffix; the path itself is still the same entry.
    let target = target.to_string_lossy();
    let target = target.strip_suffix(" (deleted)").unwrap_or(&target);
    Some(target == expected_exe)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn same_image(_pid: u32, _expected_exe: &str) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntfy_relay_core::test_support::Scratch;

    fn temp_dir(name: &str) -> Scratch {
        Scratch::new(&format!("app-lock-{name}"))
    }

    #[test]
    fn one_winner_and_a_live_pid_blocks_the_second() {
        let dir = temp_dir("winner");
        let path = dir.join("state.lock");
        let pid = std::process::id();
        assert_eq!(
            acquire_lock(&path, pid, 1_700_000_000_000).unwrap(),
            Acquired::Yes
        );
        assert_eq!(
            acquire_lock(&path, pid + 1, 1_700_000_000_000).unwrap(),
            Acquired::Running(pid)
        );
        release_lock(&path, pid);
        assert!(!path.exists());
    }

    #[test]
    fn a_dead_pid_is_replaced_once() {
        let dir = temp_dir("dead");
        let path = dir.join("state.lock");
        fs::write(&path, r#"{"pid":999999999,"started":"x"}"#).unwrap();
        let pid = std::process::id();
        assert_eq!(acquire_lock(&path, pid, 1).unwrap(), Acquired::Yes);
        assert_eq!(read_lock_body(&path).unwrap().unwrap().pid, pid);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn a_live_pid_from_another_image_is_replaced() {
        let dir = temp_dir("reused");
        let path = dir.join("state.lock");
        let mut child = spawn_sleeper();
        let child_pid = child.id();
        fs::write(&path, format!(r#"{{"pid":{child_pid},"started":"x"}}"#)).unwrap();
        let pid = std::process::id();
        assert_eq!(acquire_lock(&path, pid, 1).unwrap(), Acquired::Yes);
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(windows)]
    #[test]
    fn a_terminated_process_object_is_not_alive() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/c", "exit", "0"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let child_pid = child.id();
        child.wait().unwrap();
        // The Child handle keeps the process object and its pid allocated.
        assert!(
            !pid_alive(child_pid),
            "a waited process object is not alive"
        );
        let dir = temp_dir("terminated");
        let path = dir.join("state.lock");
        fs::write(&path, format!(r#"{{"pid":{child_pid},"started":"x"}}"#)).unwrap();
        let pid = std::process::id();
        assert_eq!(acquire_lock(&path, pid, 1).unwrap(), Acquired::Yes);
        drop(child);
    }

    #[cfg(windows)]
    fn spawn_sleeper() -> std::process::Child {
        std::process::Command::new("cmd.exe")
            .args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    fn spawn_sleeper() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn an_unreadable_lock_fails_closed() {
        let dir = temp_dir("unreadable");
        let path = dir.join("state.lock");
        fs::write(&path, "not json").unwrap();
        assert_eq!(acquire_lock(&path, 1, 1).unwrap(), Acquired::Unreadable);
        assert_eq!(
            read_lock_body(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn lock_owner_reports_this_live_image_only() {
        let dir = temp_dir("owner");
        let path = dir.join("state.lock");
        assert_eq!(lock_owner(&path).unwrap(), None);
        let pid = std::process::id();
        fs::write(&path, format!(r#"{{"pid":{pid},"started":"x"}}"#)).unwrap();
        assert_eq!(lock_owner(&path).unwrap(), Some(pid));
        fs::write(&path, r#"{"pid":999999999,"started":"x"}"#).unwrap();
        assert_eq!(lock_owner(&path).unwrap(), None);
    }

    #[test]
    fn the_lock_starts_in_the_starting_phase_and_carries_the_exe() {
        let dir = temp_dir("phase");
        let path = dir.join("state.lock");
        let pid = std::process::id();
        assert_eq!(acquire_lock(&path, pid, 1).unwrap(), Acquired::Yes);
        let body = live_lock(&path).unwrap().expect("live body");
        assert_eq!(body.pid, pid);
        assert_eq!(body.phase, PHASE_STARTING);
        assert_eq!(
            body.exe,
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        );
        set_phase(&path, pid, PHASE_READY).unwrap();
        assert_eq!(live_lock(&path).unwrap().unwrap().phase, PHASE_READY);
        set_phase(&path, pid, PHASE_READY).unwrap();
        assert_eq!(live_lock(&path).unwrap().unwrap().phase, PHASE_READY);
        assert!(!path.with_extension("lock.tmp").exists());
        release_lock(&path, pid);
    }

    #[test]
    fn a_phase_write_only_touches_the_named_pid() {
        let dir = temp_dir("phase-owner");
        let path = dir.join("state.lock");
        fs::write(
            &path,
            format!(
                r#"{{"pid":{},"started":"x","phase":"starting"}}"#,
                std::process::id()
            ),
        )
        .unwrap();
        set_phase(&path, 999_999_999, PHASE_READY).unwrap();
        assert_eq!(
            read_lock_body(&path).unwrap().unwrap().phase,
            PHASE_STARTING
        );
        set_phase(&path, std::process::id(), PHASE_READY).unwrap();
        assert_eq!(read_lock_body(&path).unwrap().unwrap().phase, PHASE_READY);
    }

    #[test]
    fn a_lock_without_a_phase_reads_as_starting() {
        let dir = temp_dir("phase-old");
        let path = dir.join("state.lock");
        fs::write(
            &path,
            format!(r#"{{"pid":{},"started":"x"}}"#, std::process::id()),
        )
        .unwrap();
        let body = read_lock_body(&path).unwrap().unwrap();
        assert_eq!(body.phase, PHASE_STARTING);
        assert_eq!(body.exe, "");
    }

    #[test]
    fn release_only_removes_the_matching_pid() {
        let dir = temp_dir("release");
        let path = dir.join("state.lock");
        fs::write(&path, r#"{"pid":1234,"started":"x"}"#).unwrap();
        release_lock(&path, 999);
        assert!(path.exists());
        release_lock(&path, 1234);
        assert!(!path.exists());
    }

    #[test]
    fn lock_path_replaces_a_json_suffix_only() {
        assert_eq!(
            lock_path_for(Path::new("/state/relay.json")),
            PathBuf::from("/state/relay.lock")
        );
        assert_eq!(
            lock_path_for(Path::new("/state/relay.JSON")),
            PathBuf::from("/state/relay.lock")
        );
        assert_eq!(
            lock_path_for(Path::new("/state/relay")),
            PathBuf::from("/state/relay.lock")
        );
    }

    #[test]
    fn lock_path_survives_a_non_ascii_suffix() {
        let path = Path::new("/state/relay-\u{e9}");
        assert_eq!(
            lock_path_for(path),
            PathBuf::from("/state/relay-\u{e9}.lock")
        );
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn a_lock_naming_another_image_path_is_not_live() {
        let dir = temp_dir("path-identity");
        let path = dir.join("state.lock");
        let pid = std::process::id();
        let other = dir.join("ntfy-relay.exe");
        fs::write(
            &path,
            json!({ "pid": pid, "started": "x", "exe": other }).to_string(),
        )
        .unwrap();
        assert_eq!(live_lock(&path).unwrap(), None);

        let current = std::env::current_exe().unwrap();
        fs::write(
            &path,
            json!({ "pid": pid, "started": "x", "exe": current }).to_string(),
        )
        .unwrap();
        assert_eq!(live_lock(&path).unwrap().unwrap().pid, pid);
    }
}
