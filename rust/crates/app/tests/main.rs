//! The `ntfy-relay` CLI contract: usage text and exit codes.
//!
//! The daemon itself is covered elsewhere; these tests spawn the binary and
//! check only what a caller can observe: 0 success, 1 error, 2 usage.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ntfy-relay"))
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ntfy-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(root: &Path, args: &[&str]) -> Output {
    binary()
        .args(args)
        .env("NTFY_RELAY_CONFIG", root.join("ntfy-relay.json"))
        .env("NTFY_PODMAN_BIN", root.join("missing-podman"))
        .env("NTFY_TAILSCALE_BIN", root.join("missing-tailscale"))
        .env("NTFY_HEALTH_URL", "http://127.0.0.1:1/v1/health")
        .output()
        .expect("spawn ntfy-relay")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn no_argument_prints_the_usage_and_exits_two() {
    let root = temp("no-arg");
    let output = run(&root, &[]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("usage: ntfy-relay"), "{output:?}");
    assert!(stdout(&output).is_empty(), "{output:?}");
}

#[test]
fn a_bad_argument_prints_the_usage_and_exits_two() {
    let root = temp("bad-arg");
    let output = run(&root, &["bogus"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("usage: ntfy-relay"), "{output:?}");
}

#[test]
fn help_prints_the_usage_and_exits_zero() {
    let root = temp("help");
    for help in ["help", "--help", "-h"] {
        let output = run(&root, &[help]);
        assert_eq!(output.status.code(), Some(0), "{help}: {output:?}");
        assert!(stdout(&output).contains("usage: ntfy-relay"), "{output:?}");
    }
}

#[test]
fn a_non_unicode_argument_prints_the_usage_and_exits_two() {
    let root = temp("non-unicode");
    let output = binary()
        .arg(non_unicode_argument())
        .env("NTFY_RELAY_CONFIG", root.join("ntfy-relay.json"))
        .output()
        .expect("spawn ntfy-relay");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("usage: ntfy-relay"), "{output:?}");
}

#[cfg(unix)]
fn non_unicode_argument() -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(b"\xff".to_vec())
}

#[cfg(windows)]
fn non_unicode_argument() -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    // A lone surrogate is not a Unicode scalar value.
    std::ffi::OsString::from_wide(&[0xD800])
}

#[test]
fn status_exits_one_when_the_config_cannot_be_read() {
    let root = temp("status-missing");
    let output = run(&root, &["status"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr(&output).starts_with("FAIL "), "{output:?}");
}

#[test]
fn status_exits_zero_even_when_rows_fail() {
    let root = temp("status-good");
    std::fs::write(
        root.join("ntfy-relay.json"),
        br#"{"ntfy":{"server":"http://127.0.0.1:8810","tokenFile":"ntfy-token.txt"},"projectRoots":[]}"#,
    )
    .unwrap();
    let output = run(&root, &["status"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("config "), "{output:?}");
    assert!(
        text.contains("FAIL ") || text.contains("SKIP "),
        "{output:?}"
    );
}
