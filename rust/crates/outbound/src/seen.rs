//! Durable event-id window shared by live events and the reconnect backfill.
//!
//! The in-memory window alone forgets every id when the daemon restarts, so a
//! reconnect backfill inside its window would publish the last turns again. The
//! window is journaled to one append-only JSON-lines file: an id is committed
//! after its effects, so a crash can repeat an effect but never skip one. The
//! file is compacted on load and when it grows.
//!
//! The journal is an optimization. An open or write failure is logged once and
//! leaves the in-memory window running; dedupe across a restart is at-least-once
//! again, and the outbound task never stops for a state problem.

use std::collections::{HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::log::Log;

/// File name next to the relay state file.
pub const SEEN_FILE: &str = "ntfy-relay-seen.jsonl";
/// Seen window and cold-start backfill range.
pub const RETENTION_MS: i64 = 15 * 60 * 1000;
/// Same bound as the in-memory window.
pub const MAX_IDS: usize = 8192;
const COMPACT_BYTES: u64 = 1024 * 1024;
const COMPACT_LINES: u64 = 16_384;

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    id: String,
    at: i64,
}

/// Bounded window of handled event ids, optionally backed by a journal.
pub struct SeenEvents {
    ids: HashSet<String>,
    order: VecDeque<(String, i64)>,
    journal: Option<Journal>,
}

struct Journal {
    path: PathBuf,
    file: Option<File>,
    bytes: u64,
    lines: u64,
}

impl SeenEvents {
    /// Loads the journal when a path is given. Expired and malformed lines are
    /// dropped, the result is compacted, and the append handle stays open.
    pub fn open(path: Option<PathBuf>, now: i64, log: &dyn Log) -> Self {
        let mut seen = Self {
            ids: HashSet::new(),
            order: VecDeque::new(),
            journal: None,
        };
        let Some(path) = path else {
            return seen;
        };
        match read_records(&path) {
            Ok(records) => {
                for (id, at) in records {
                    if at >= now - RETENTION_MS {
                        seen.insert(&id, at);
                    }
                }
            }
            Err(error) => log.warn(&format!(
                "seen journal {}: {error}; starting empty",
                path.display()
            )),
        }
        match Journal::open(&path) {
            Ok(mut journal) => {
                if let Err(error) = journal.compact(&seen.order) {
                    log.warn(&format!(
                        "seen journal {}: {error}; old lines are kept",
                        path.display()
                    ));
                }
                seen.journal = Some(journal);
            }
            Err(error) => log.warn(&format!(
                "seen journal {}: {error}; duplicates across a restart are possible",
                path.display()
            )),
        }
        seen
    }

    /// True when the id was not handled before. The id is committed only after
    /// its effects have been attempted, so a crash repeats, never loses, a push.
    pub fn accept(&mut self, id: &str, now: i64) -> bool {
        self.prune(now);
        if self.ids.contains(id) {
            return false;
        }
        self.insert(id, now);
        true
    }

    /// Peek without marking. Backfill uses it to skip a message a live part
    /// already delivered.
    pub fn contains(&mut self, id: &str, now: i64) -> bool {
        self.prune(now);
        self.ids.contains(id)
    }

    /// Appends a handled id and compacts when the journal outgrows its bounds.
    /// Compaction rewrites the whole file, so it runs on a blocking thread.
    pub async fn commit(&mut self, id: &str, at: i64, log: &dyn Log) {
        let mut compact = false;
        if let Some(journal) = self.journal.as_mut() {
            if let Err(error) = journal.append(id, at) {
                log.warn(&format!(
                    "seen journal {}: {error}; duplicates across a restart are possible",
                    journal.path.display()
                ));
                self.journal = None;
                return;
            }
            compact = journal.bytes >= COMPACT_BYTES || journal.lines >= COMPACT_LINES;
        }
        if !compact {
            return;
        }
        let Some(mut journal) = self.journal.take() else {
            return;
        };
        let path = journal.path.display().to_string();
        let records = self.order.clone();
        let result =
            tokio::task::spawn_blocking(move || (journal.compact(&records), journal)).await;
        match result {
            Ok((Ok(()), journal)) => self.journal = Some(journal),
            Ok((Err(error), journal)) => {
                log.warn(&format!(
                    "seen journal {}: {error}; journal is left to grow",
                    journal.path.display()
                ));
                self.journal = Some(journal);
            }
            Err(error) => log.warn(&format!(
                "seen journal {path}: compaction task failed: {error}; journal is closed"
            )),
        }
    }

    /// Removes an id from the window. A transient lookup failure unmarks the
    /// event so a later backfill can recover it; nothing was journaled yet.
    pub fn forget(&mut self, id: &str) {
        if !self.ids.remove(id) {
            return;
        }
        if let Some(index) = self.order.iter().position(|(held, _)| held == id) {
            self.order.remove(index);
        }
    }

    fn insert(&mut self, id: &str, at: i64) {
        if !self.ids.insert(id.to_string()) {
            return;
        }
        self.order.push_back((id.to_string(), at));
        while self.order.len() > MAX_IDS {
            if let Some((oldest, _)) = self.order.pop_front() {
                self.ids.remove(&oldest);
            }
        }
    }

    fn prune(&mut self, now: i64) {
        let cutoff = now - RETENTION_MS;
        while let Some((_, seen_at)) = self.order.front() {
            if *seen_at >= cutoff {
                break;
            }
            if let Some((oldest, _)) = self.order.pop_front() {
                self.ids.remove(&oldest);
            }
        }
    }
}

impl Journal {
    fn open(path: &Path) -> Result<Self, String> {
        let file = open_file(path)?;
        let bytes = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(file),
            bytes,
            lines: 0,
        })
    }

    fn append(&mut self, id: &str, at: i64) -> Result<(), String> {
        let mut line = serde_json::to_vec(&Record {
            id: id.to_string(),
            at,
        })
        .map_err(|error| error.to_string())?;
        line.push(b'\n');
        let file = self.file.as_mut().ok_or("journal is closed")?;
        file.seek(SeekFrom::End(0))
            .map_err(|error| error.to_string())?;
        file.write_all(&line).map_err(|error| error.to_string())?;
        self.bytes += line.len() as u64;
        self.lines += 1;
        Ok(())
    }

    /// Replaces the file with the current window: temp file, then one rename.
    /// The handle is closed first, because Windows refuses a replace under it.
    fn compact(&mut self, records: &VecDeque<(String, i64)>) -> Result<(), String> {
        let mut body = Vec::with_capacity(records.len() * 48);
        for (id, at) in records {
            serde_json::to_writer(
                &mut body,
                &Record {
                    id: id.clone(),
                    at: *at,
                },
            )
            .map_err(|error| error.to_string())?;
            body.push(b'\n');
        }
        self.file = None;
        let written = write_atomic(&self.path, &body);
        let reopened = open_file(&self.path);
        match (written, reopened) {
            (Ok(()), Ok(file)) => {
                self.file = Some(file);
                self.bytes = body.len() as u64;
                self.lines = records.len() as u64;
                Ok(())
            }
            (Err(error), Ok(file)) => {
                self.file = Some(file);
                Err(error)
            }
            (_, Err(error)) => Err(error),
        }
    }
}

fn open_file(path: &Path) -> Result<File, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

fn write_atomic(path: &Path, body: &[u8]) -> Result<(), String> {
    let temp = temp_path(path);
    let mut handle =
        ntfy_relay_core::fsutil::owner_only(&temp).map_err(|error| error.to_string())?;
    handle.write_all(body).map_err(|error| error.to_string())?;
    handle.sync_all().map_err(|error| error.to_string())?;
    drop(handle);
    fs::rename(&temp, path).map_err(|error| error.to_string())?;
    ntfy_relay_core::fsutil::sync_parent(path);
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let mut temp = path.as_os_str().to_os_string();
    temp.push(format!(".{}.tmp", std::process::id()));
    PathBuf::from(temp)
}

/// Reads every well-formed record; a missing file is an empty journal.
fn read_records(path: &Path) -> Result<Vec<(String, i64)>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut records = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|error| error.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<Record>(&line) {
            records.push((record.id, record.at));
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("outbound-seen-tests")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_duplicate_is_rejected_immediately() {
        let mut seen = SeenEvents::open(None, 1_000, &crate::log::NoopLog);
        assert!(seen.accept("e1", 1_000));
        assert!(!seen.accept("e1", 1_000));
    }

    #[cfg(unix)]
    #[test]
    fn the_journal_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("mode");
        let probe = root.join("probe");
        fs::write(&probe, b"x").unwrap();
        if fs::metadata(&probe).unwrap().permissions().mode() & 0o022 != 0 {
            return;
        }
        let handle = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        handle.block_on(async {
            let path = root.join(SEEN_FILE);
            let mut seen = SeenEvents::open(Some(path.clone()), 1_000, &crate::log::NoopLog);
            assert!(seen.accept("e1", 1_000));
            seen.commit("e1", 1_000, &crate::log::NoopLog).await;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        });
    }

    #[test]
    fn a_peek_does_not_mark_the_id() {
        let mut seen = SeenEvents::open(None, 1_000, &crate::log::NoopLog);
        assert!(!seen.contains("msg_1", 1_000));
        assert!(seen.accept("msg_1", 1_000));
        assert!(seen.contains("msg_1", 1_000));
        assert!(!seen.contains("msg_2", 1_000));
        assert!(seen.accept("msg_2", 1_000));
    }

    #[test]
    fn the_window_evicts_the_oldest_id() {
        let mut seen = SeenEvents::open(None, 1_000, &crate::log::NoopLog);
        for index in 0..=MAX_IDS {
            assert!(seen.accept(&format!("e{index}"), 1_000));
        }
        assert_eq!(seen.ids.len(), MAX_IDS);
        assert_eq!(seen.order.len(), MAX_IDS);
        assert!(seen.accept("e0", 1_000), "the first id was evicted");
    }

    #[test]
    fn an_expired_id_is_accepted_again() {
        let now = 10_000_000;
        let mut seen = SeenEvents::open(None, now, &crate::log::NoopLog);
        assert!(seen.accept("old", now - RETENTION_MS - 1));
        assert!(seen.accept("old", now));
    }

    #[test]
    fn expired_and_malformed_lines_are_dropped_on_load() {
        let root = temp_root("load");
        let path = root.join(SEEN_FILE);
        let now = 10_000_000;
        let body = format!(
            "{{\"id\":\"live\",\"at\":{now}}}\nnot json\n{{\"id\":\"old\",\"at\":{}}}\n",
            now - RETENTION_MS - 1
        );
        fs::write(&path, body).unwrap();
        let mut seen = SeenEvents::open(Some(path.clone()), now, &crate::log::NoopLog);
        assert!(!seen.accept("live", now));
        assert!(seen.accept("old", now));
        let compacted = fs::read_to_string(&path).unwrap();
        assert_eq!(compacted.lines().count(), 1);
        assert!(compacted.contains("\"id\":\"live\""));
    }

    #[test]
    fn a_committed_id_survives_a_reopen() {
        let root = temp_root("reopen");
        let path = root.join(SEEN_FILE);
        let now = 10_000_000;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut seen = SeenEvents::open(Some(path.clone()), now, &crate::log::NoopLog);
            assert!(seen.accept("e1", now));
            seen.commit("e1", now, &crate::log::NoopLog).await;
        });
        let mut reopened = SeenEvents::open(Some(path), now + 1_000, &crate::log::NoopLog);
        assert!(!reopened.accept("e1", now + 1_000));
    }

    #[test]
    fn a_forgotten_id_is_accepted_again() {
        let mut seen = SeenEvents::open(None, 1_000, &crate::log::NoopLog);
        assert!(seen.accept("e1", 1_000));
        seen.forget("e1");
        assert!(seen.accept("e1", 1_000));
        assert_eq!(seen.order.len(), 1);
    }

    #[test]
    fn an_uncommitted_id_is_accepted_after_a_reopen() {
        let root = temp_root("uncommitted");
        let path = root.join(SEEN_FILE);
        let now = 10_000_000;
        let mut seen = SeenEvents::open(Some(path.clone()), now, &crate::log::NoopLog);
        assert!(seen.accept("e1", now));
        drop(seen);
        let mut reopened = SeenEvents::open(Some(path), now + 1_000, &crate::log::NoopLog);
        assert!(reopened.accept("e1", now + 1_000));
    }

    #[tokio::test]
    async fn a_large_commit_compacts_without_losing_the_window() {
        let root = temp_root("compact");
        let path = root.join(SEEN_FILE);
        let now = 10_000_000;
        let mut seen = SeenEvents::open(Some(path.clone()), now, &crate::log::NoopLog);
        let big = "x".repeat(4096);
        for index in 0..300 {
            let id = format!("{big}-{index}");
            assert!(seen.accept(&id, now));
            seen.commit(&id, now, &crate::log::NoopLog).await;
        }
        drop(seen);
        let mut reopened = SeenEvents::open(Some(path), now + 1_000, &crate::log::NoopLog);
        assert!(!reopened.accept(&format!("{big}-299"), now + 1_000));
    }
}
