//! Inbox retention.
//!
//! Prune `<project>\.ntfy-inbox` at startup and every six hours. Only regular
//! files named `<epoch>-<name>` older than seven days are removed; the epoch in
//! the name is the age, with the modification time as a fallback. Symlinks,
//! directories, renamed files, and unmatched names stay untouched.
//!
//! Mirrors `ntfy-inbox-prune.mjs`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const INBOX_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;
pub const INBOX_PRUNE_INTERVAL_MS: i64 = 6 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneResult {
    pub removed: u64,
    pub bytes: u64,
    pub failed: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PruneTotals {
    pub roots: u64,
    pub removed: u64,
    pub bytes: u64,
    pub failed: u64,
    /// Roots whose inbox could not be listed. The app logs one warning each.
    pub failed_roots: Vec<(PathBuf, String)>,
}

/// `^[0-9]{10}([0-9]{3})?-[^/]*`
pub fn matches_inbox_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 11 || !bytes[..10].iter().all(u8::is_ascii_digit) {
        return false;
    }
    if bytes[10] == b'-' {
        return true;
    }
    bytes.len() >= 14 && bytes[10..13].iter().all(u8::is_ascii_digit) && bytes[13] == b'-'
}

/// The epoch in an inbox name, in milliseconds; ten digits are seconds. The
/// name is the age, so a copy keeps its original age and a touched file does
/// not grow younger.
fn inbox_stamp_ms(name: &str) -> Option<i64> {
    let (digits, _) = name.split_once('-')?;
    let value = digits.parse::<i64>().ok()?;
    Some(if digits.len() == 10 {
        value.saturating_mul(1000)
    } else {
        value
    })
}

fn modified_ms(metadata: &fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis() as i64)
}

pub fn prune_inbox(dir: &Path, now_ms: i64, retention_ms: i64) -> io::Result<PruneResult> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PruneResult::default());
        }
        Err(error) => return Err(error),
    };

    let cutoff = now_ms - retention_ms;
    let mut result = PruneResult::default();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                result.failed += 1;
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !matches_inbox_name(name) {
            continue;
        }
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                result.failed += 1;
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let stamp_ms = match inbox_stamp_ms(name).or_else(|| modified_ms(&metadata)) {
            Some(stamp_ms) => stamp_ms,
            None => {
                result.failed += 1;
                continue;
            }
        };
        if stamp_ms >= cutoff {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                result.removed += 1;
                result.bytes += metadata.len();
            }
            Err(_) => result.failed += 1,
        }
    }
    Ok(result)
}

pub fn prune_inboxes(
    roots: &[PathBuf],
    inbox_dir: &str,
    now_ms: i64,
    retention_ms: i64,
) -> PruneTotals {
    let mut totals = PruneTotals::default();
    for root in roots {
        totals.roots += 1;
        match prune_inbox(&root.join(inbox_dir), now_ms, retention_ms) {
            Ok(result) => {
                totals.removed += result.removed;
                totals.bytes += result.bytes;
                totals.failed += result.failed;
            }
            Err(error) => {
                totals.failed += 1;
                totals.failed_roots.push((root.clone(), error.to_string()));
            }
        }
    }
    totals
}

/// Bytes of regular files directly under one inbox. A missing inbox is zero;
/// symlinks and directories are not counted.
pub fn inbox_bytes(dir: &Path) -> io::Result<u64> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut total = 0u64;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.is_file() {
            total += metadata.len();
        }
    }
    Ok(total)
}

/// Makes room for `need` bytes in one inbox by deleting the oldest matching
/// `<epoch>-<name>` regular files. Returns the remaining byte total. A file is
/// only removed when `used + need` still exceeds `budget`; unmatched names,
/// directories, and symlinks stay. The 200 MB budget in `relay` calls this
/// before a fetch and again after it to trim an overshoot.
pub fn make_room(dir: &Path, need: u64, budget: u64) -> io::Result<u64> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut files: Vec<(i64, u64, PathBuf)> = Vec::new();
    let mut used = 0u64;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !matches_inbox_name(name) {
            continue;
        }
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }
        let age_ms = inbox_stamp_ms(name)
            .or_else(|| modified_ms(&metadata))
            .unwrap_or(0);
        used += metadata.len();
        files.push((age_ms, metadata.len(), path));
    }
    if used.saturating_add(need) <= budget {
        return Ok(used);
    }
    files.sort_by_key(|(age_ms, _, _)| *age_ms);
    for (_, len, path) in files {
        if used.saturating_add(need) <= budget {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            used = used.saturating_sub(len);
        }
    }
    Ok(used)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntfy_relay_core::test_support::Scratch;
    use std::fs::File;
    use std::time::{Duration, SystemTime};

    const DAY: i64 = 24 * 60 * 60 * 1000;

    fn temp_dir(name: &str) -> Scratch {
        Scratch::new(&format!("inbound-prune-{name}"))
    }

    fn write_aged(file: &Path, body: &[u8], age_ms: i64, now_ms: i64) {
        fs::write(file, body).unwrap();
        let at = SystemTime::UNIX_EPOCH + Duration::from_millis((now_ms - age_ms).max(0) as u64);
        let handle = File::options().write(true).open(file).unwrap();
        handle
            .set_times(fs::FileTimes::new().set_modified(at))
            .unwrap();
    }

    #[test]
    fn name_pattern_accepts_epoch_seconds_and_millis_only() {
        assert!(matches_inbox_name("1700000000-report.pdf"));
        assert!(matches_inbox_name("1700000000123-photo.png"));
        assert!(!matches_inbox_name("170000000-report.pdf"));
        assert!(!matches_inbox_name("17000000001234-photo.png"));
        assert!(!matches_inbox_name("report.pdf"));
        assert!(!matches_inbox_name(".hidden"));
    }

    #[test]
    fn removes_only_aged_matching_regular_files() {
        let dir = temp_dir("aged");
        let now = 1_700_000_000_000;
        let old = dir.join("1699308800-old.pdf");
        let fresh = dir.join("1700000000123-fresh.pdf");
        let odd = dir.join("notes.txt");
        let sub = dir.join("1700000001-dir");
        write_aged(&old, b"0123456789", 8 * DAY, now);
        write_aged(&fresh, b"0123456789", DAY, now);
        write_aged(&odd, b"0123456789", 8 * DAY, now);
        fs::create_dir(&sub).unwrap();

        let result = prune_inbox(&dir, now, INBOX_RETENTION_MS).unwrap();
        assert_eq!(
            result,
            PruneResult {
                removed: 1,
                bytes: 10,
                failed: 0
            }
        );
        assert!(!old.exists());
        assert!(fresh.is_file());
        assert!(odd.is_file());
        assert!(sub.is_dir());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_name_decides_the_age_not_the_mtime() {
        let dir = temp_dir("name-age");
        let now = 1_700_000_000_000;
        let copied = dir.join("1699308800-copied.pdf");
        let touched = dir.join("1700000000123-touched.pdf");
        fs::write(&copied, b"old").unwrap();
        write_aged(&touched, b"new", 30 * DAY, now);

        let result = prune_inbox(&dir, now, INBOX_RETENTION_MS).unwrap();
        assert_eq!(result.removed, 1);
        assert!(!copied.exists(), "a fresh mtime does not keep an old name");
        assert!(touched.is_file(), "an old mtime does not age a fresh name");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_directory_is_a_no_op() {
        let dir = temp_dir("missing");
        let result =
            prune_inbox(&dir.join("absent"), 1_700_000_000_000, INBOX_RETENTION_MS).unwrap();
        assert_eq!(result, PruneResult::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn symlinks_are_not_followed_or_removed() {
        let dir = temp_dir("symlink");
        let now = 1_700_000_000_000;
        let target = dir.join("target.txt");
        let link = dir.join("1700000000-link.txt");
        write_aged(&target, b"keep", 30 * DAY, now);
        let linked = link_file(&target, &link);
        if !linked {
            fs::remove_dir_all(&dir).unwrap();
            return;
        }

        let result = prune_inbox(&dir, now, INBOX_RETENTION_MS).unwrap();
        assert_eq!(result.removed, 0);
        assert!(target.is_file());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    fn link_file(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(windows)]
    fn link_file(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }

    #[test]
    fn one_bad_root_is_counted_and_the_run_continues() {
        let dir = temp_dir("roots");
        let now = 1_700_000_000_000;
        let good = dir.join("good");
        let bad = dir.join("bad");
        fs::create_dir_all(good.join(".ntfy-inbox")).unwrap();
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join(".ntfy-inbox"), b"not a directory").unwrap();
        write_aged(
            &good.join(".ntfy-inbox").join("1699308800-old.pdf"),
            b"abcd",
            8 * DAY,
            now,
        );

        let totals = prune_inboxes(&[good, bad.clone()], ".ntfy-inbox", now, INBOX_RETENTION_MS);
        assert_eq!(totals.roots, 2);
        assert_eq!(totals.removed, 1);
        assert_eq!(totals.bytes, 4);
        assert_eq!(totals.failed, 1);
        assert_eq!(totals.failed_roots.len(), 1);
        assert_eq!(totals.failed_roots[0].0, bad);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn inbox_bytes_counts_regular_files_only() {
        let dir = temp_dir("bytes");
        assert_eq!(inbox_bytes(&dir.join("absent")).unwrap(), 0);
        fs::write(dir.join("a.bin"), b"12345").unwrap();
        fs::write(dir.join("b.bin"), b"123").unwrap();
        fs::create_dir(dir.join("1700000001-dir")).unwrap();
        assert_eq!(inbox_bytes(&dir).unwrap(), 8);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn make_room_evicts_the_oldest_matching_files_first() {
        let dir = temp_dir("room");
        let now = 1_700_000_000_000;
        let oldest = dir.join("1700000000-oldest.bin");
        let newer = dir.join("1700000001-newer.bin");
        let odd = dir.join("keep.txt");
        write_aged(&oldest, &[0u8; 10], 20 * DAY, now);
        write_aged(&newer, &[0u8; 10], 10 * DAY, now);
        write_aged(&odd, &[0u8; 10], 30 * DAY, now);

        let left = make_room(&dir, 5, 15).unwrap();
        assert_eq!(left, 10, "the oldest matching file is removed");
        assert!(!oldest.exists());
        assert!(newer.is_file());
        assert!(odd.is_file(), "unmatched names are never removed");

        let left = make_room(&dir, 10, 15).unwrap();
        assert_eq!(left, 0, "the second file is removed when still over");
        assert!(!newer.exists());
        assert!(odd.is_file());
        fs::remove_dir_all(&dir).unwrap();
    }
}
