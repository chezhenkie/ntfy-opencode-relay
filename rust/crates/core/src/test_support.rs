//! Scratch directories for tests.
//!
//! One guard creates a unique directory under the system temp and removes it
//! on `Drop`, so two test runs do not share a path and a panicking test leaves
//! nothing behind.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A directory named `ntfy-test-<pid>-<counter>-<label>`, removed on drop.
/// `path` is canonical, so tests that reject `..` see the resolved form.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub fn new(label: &str) -> Self {
        loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ntfy-test-{}-{id}-{}",
                std::process::id(),
                sanitize(label)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    let path = std::fs::canonicalize(&path)
                        .unwrap_or_else(|error| panic!("resolve {}: {error}", path.display()));
                    return Self { path: plain(path) };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    // A crashed earlier run can hold the name; replace it.
                    let _ = std::fs::remove_dir_all(&path);
                }
                Err(error) => panic!("create {}: {error}", path.display()),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn sanitize(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "scratch".to_string()
    } else {
        cleaned
    }
}

/// `\\?\C:\...` is the verbatim form; the relay's path policy works on the
/// plain form, so strip the prefix here.
fn plain(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    if let Some(rest) = path.to_string_lossy().strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_guards_get_separate_directories() {
        let one = Scratch::new("same");
        let two = Scratch::new("same");
        assert_ne!(one.path(), two.path());
        assert!(one.is_dir() && two.is_dir());
    }

    #[test]
    fn the_directory_is_gone_after_drop() {
        let path = {
            let scratch = Scratch::new("drop");
            scratch.path().to_path_buf()
        };
        assert!(!path.exists());
    }

    #[test]
    fn a_panic_in_the_test_still_removes_the_directory() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot = std::sync::Arc::clone(&seen);
        let result = std::panic::catch_unwind(move || {
            let scratch = Scratch::new("panic");
            *slot.lock().unwrap() = Some(scratch.path().to_path_buf());
            panic!("the test unwinds");
        });
        assert!(result.is_err());
        let path = seen.lock().unwrap().take().expect("path was recorded");
        assert!(!path.exists(), "{} survived the panic", path.display());
    }

    #[test]
    fn a_label_with_separators_cannot_escape_the_temp_parent() {
        let scratch = Scratch::new("a/b\\c..");
        assert_eq!(scratch.parent(), Some(std::env::temp_dir().as_path()));
        assert!(scratch
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("ntfy-test-"));
    }
}
