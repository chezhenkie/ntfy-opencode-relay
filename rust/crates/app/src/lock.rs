//! Single-relay lock.
//!
//! Mirrors `acquireLock` and `releaseLock` in `ntfy-inbox-relay.mjs`: create
//! `wx`, write the pid, and on contention read the existing file. A live pid
//! from the same image wins, a dead or reused pid is replaced once, and an
//! unreadable file fails closed.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use ntfy_relay_control::process::pid_alive;
use serde_json::{json, Value};

use crate::logger::format_timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquired {
    Yes,
    Running(u32),
    Unreadable,
    Contended,
}

pub fn lock_path_for(state_file: &Path) -> PathBuf {
    let text = state_file.to_string_lossy();
    let stem = if text.len() >= 5 && text[text.len() - 5..].eq_ignore_ascii_case(".json") {
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
    let body = json!({
        "pid": pid,
        "started": format_timestamp(now_ms),
    })
    .to_string();

    for _attempt in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut handle) => {
                handle.write_all(body.as_bytes())?;
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
        let Some(existing_pid) = existing
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
        else {
            return Ok(Acquired::Unreadable);
        };
        if existing_pid != pid && pid_is_relay(existing_pid) {
            return Ok(Acquired::Running(existing_pid));
        }
        fs::remove_file(path)?;
    }
    Ok(Acquired::Contended)
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

pub fn read_lock_pid(path: &Path) -> io::Result<Option<u32>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(value
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0))
}

/// The lock pid when it names a live relay image. A dead pid, a pid reused by
/// another program, or a missing lock reads as `None`; a corrupt file is an
/// error, because a second relay would fail closed on it too.
pub fn lock_owner(path: &Path) -> io::Result<Option<u32>> {
    match read_lock_pid(path)? {
        Some(pid) if pid_is_relay(pid) => Ok(Some(pid)),
        _ => Ok(None),
    }
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

/// A live pid only blocks the lock when it is this relay image. Windows can
/// reuse a pid right after a hard kill; without the image check the lock looks
/// live and the next start exits until that unrelated process ends.
fn pid_is_relay(pid: u32) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    same_image(pid).unwrap_or(true)
}

/// `None` means the image could not be read; the caller must stay conservative.
#[cfg(windows)]
fn same_image(pid: u32) -> Option<bool> {
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
    Some(image_name(&target)? == image_name(&current.to_string_lossy())?)
}

#[cfg(windows)]
fn image_name(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
}

#[cfg(target_os = "linux")]
fn same_image(pid: u32) -> Option<bool> {
    let target = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let current = std::env::current_exe().ok()?;
    Some(target.file_name()? == current.file_name()?)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn same_image(_pid: u32) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("app-lock-tests")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
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
        assert_eq!(read_lock_pid(&path).unwrap(), Some(pid));
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
            read_lock_pid(&path).unwrap_err().kind(),
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
}
