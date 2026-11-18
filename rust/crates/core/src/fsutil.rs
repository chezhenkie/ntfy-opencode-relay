//! Durable file helpers shared by the state file and the seen journal.

use std::fs;
use std::io;
use std::path::Path;

/// Opens a file for writing with owner-only mode on Unix.
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
