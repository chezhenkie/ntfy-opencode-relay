//! The runtime file that tells `ntfy-admin` where the endpoint listens.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const CONTROL_FILE: &str = "ntfy-relay-endpoint.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlFile {
    pub pid: u32,
    pub port: u16,
    pub token: String,
}

impl ControlFile {
    pub fn path(root: &Path) -> PathBuf {
        root.join(CONTROL_FILE)
    }

    /// Read the file; a missing or malformed file means no usable endpoint.
    pub fn read(root: &Path) -> Option<Self> {
        let bytes = fs::read(Self::path(root)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Write through a temporary sibling so a failure leaves the old file.
    pub fn write(&self, root: &Path) -> io::Result<()> {
        let path = Self::path(root);
        let temp = path.with_extension("json.tmp");
        let text = serde_json::to_vec(self).map_err(io::Error::other)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(&text)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &path)
    }

    /// Best effort; a crash leaves the file for the next start to replace.
    pub fn remove(root: &Path) {
        let _ = fs::remove_file(Self::path(root));
    }
}

/// A fresh token per process, 32 lowercase hex. `RandomState` is seeded from
/// OS entropy, so the value does not follow the clock.
pub fn new_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut value = String::with_capacity(32);
    for round in 0..2u8 {
        let mut hasher = RandomState::new().build_hasher();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|span| span.as_nanos())
            .unwrap_or(0);
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        hasher.write_u8(round);
        value.push_str(&format!("{:016x}", hasher.finish()));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn file() -> ControlFile {
        ControlFile {
            pid: 42,
            port: 51234,
            token: new_token(),
        }
    }

    #[test]
    fn tokens_are_hex_and_differ() {
        let first = new_token();
        let second = new_token();
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn write_read_and_remove() {
        let root = TempDir::new("runtime");
        let value = file();
        value.write(root.path()).expect("write");
        assert_eq!(ControlFile::read(root.path()), Some(value.clone()));
        assert!(!root.path().join("ntfy-relay-endpoint.json.tmp").exists());
        ControlFile::remove(root.path());
        assert_eq!(ControlFile::read(root.path()), None);
    }

    #[test]
    fn malformed_file_reads_as_none() {
        let root = TempDir::new("runtime-malformed");
        fs::write(ControlFile::path(root.path()), b"not json").expect("write");
        assert_eq!(ControlFile::read(root.path()), None);
    }

    #[cfg(unix)]
    #[test]
    fn file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = TempDir::new("runtime-mode");
        file().write(root.path()).expect("write");
        let mode = fs::metadata(ControlFile::path(root.path()))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
