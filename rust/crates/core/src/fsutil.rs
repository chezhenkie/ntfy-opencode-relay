//! Durable file helpers shared by the state file and the seen journal.

use std::fs;
use std::io::{self, Read};
use std::path::Path;

/// Outcome of [`read_bounded`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundedRead {
    /// The path does not exist.
    Missing,
    /// The file content, at most the cap.
    Text(String),
    /// The file is larger than the cap.
    Oversize,
    /// The file is not valid UTF-8.
    NotText,
}

/// Reads a text file while holding at most `cap + 1` bytes. A file over the cap
/// is reported instead of truncated, so its content can be quarantined rather
/// than overwritten.
pub fn read_bounded(path: &Path, cap: usize) -> io::Result<BoundedRead> {
    let handle = match fs::File::open(path) {
        Ok(handle) => handle,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BoundedRead::Missing),
        Err(error) => return Err(error),
    };
    let mut buffer = Vec::with_capacity(cap.min(64 * 1024).saturating_add(1));
    handle
        .take(cap.saturating_add(1) as u64)
        .read_to_end(&mut buffer)?;
    if buffer.len() > cap {
        return Ok(BoundedRead::Oversize);
    }
    match String::from_utf8(buffer) {
        Ok(text) => Ok(BoundedRead::Text(text)),
        Err(_) => Ok(BoundedRead::NotText),
    }
}

/// Opens a file for writing with owner-only mode on Unix. Windows has no
/// owner-only mode: the file relies on the install root's inherited per-user
/// ACL, and `ntfy-deploy install` warns when the root is elsewhere.
pub fn owner_only(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let handle = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        handle.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(handle)
}

/// Makes the rename durable on Unix. A directory handle cannot be opened on
/// Windows, and NTFS metadata ordering is left to the OS.
pub fn sync_parent(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let Ok(handle) = fs::File::open(parent) {
            let _ = handle.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("core-fsutil-unit")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_missing_file_is_reported_not_failed() {
        let dir = temp_dir("missing");
        assert_eq!(
            read_bounded(&dir.join("absent.json"), 16).unwrap(),
            BoundedRead::Missing
        );
    }

    #[test]
    fn a_file_at_the_cap_reads_and_one_byte_more_is_oversize() {
        let dir = temp_dir("cap");
        let file = dir.join("state.json");
        fs::write(&file, vec![b'x'; 16]).unwrap();
        assert_eq!(
            read_bounded(&file, 16).unwrap(),
            BoundedRead::Text("x".repeat(16))
        );
        fs::write(&file, vec![b'x'; 17]).unwrap();
        assert_eq!(read_bounded(&file, 16).unwrap(), BoundedRead::Oversize);
        assert_eq!(fs::metadata(&file).unwrap().len(), 17);
    }

    #[test]
    fn invalid_utf8_is_reported_and_left_in_place() {
        let dir = temp_dir("not-text");
        let file = dir.join("asks.json");
        fs::write(&file, [0x7b, 0xff, 0x7d]).unwrap();
        assert_eq!(read_bounded(&file, 16).unwrap(), BoundedRead::NotText);
        assert!(file.exists());
    }
}
