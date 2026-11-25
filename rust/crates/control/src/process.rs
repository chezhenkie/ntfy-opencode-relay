//! Process liveness for the runtime file.
//!
//! The daemon uses it to decide whether a lock owner still exists; the CLI
//! uses it to tell a stale control file from a running relay, and to wait for
//! a stop. A pid from a file is not trusted: `0` and a value beyond the OS
//! range read as dead.

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        true
    } else {
        // A process owned by another user answers EPERM, which proves it exists.
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return false;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        // Access denied still proves the process exists.
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    }
    let mut code = 0u32;
    let queried = unsafe { GetExitCodeProcess(handle, &mut code) };
    unsafe {
        CloseHandle(handle);
    }
    // A killed process keeps its pid while a parent still holds the handle.
    // Its exit code is no longer STILL_ACTIVE, so it must not count as alive.
    // A failed query stays conservative: alive, never a second relay.
    queried == 0 || code == STILL_ACTIVE as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_the_current_pid() {
        assert!(!pid_alive(0));
        assert!(pid_alive(std::process::id()));
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn a_finished_child_is_not_alive() {
        let mut child = sleeper();
        let pid = child.id();
        assert!(pid_alive(pid));
        let _ = child.kill();
        let _ = child.wait();
        assert!(!pid_alive(pid));
    }

    #[cfg(windows)]
    fn sleeper() -> std::process::Child {
        std::process::Command::new("cmd.exe")
            .args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    fn sleeper() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }
}
