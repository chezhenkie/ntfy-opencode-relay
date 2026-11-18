//! Registry of permission asks that reached the phone.
//!
//! A record is written before its push is published, so a phone reply resolves
//! the ask that was seen instead of a routed session. Records are bound to the
//! OpenCode request id and session id; the short code is a prefix of the
//! request id's sha256 digest, extended only on collision.
//!
//! The file is owned by the daemon, written with the same temp-file rename as
//! the state file, and quarantined on corruption. Tombstones for a desktop
//! answer are in memory only and expire after 15 minutes.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// File name next to the relay state file.
pub const ASK_FILE: &str = "ntfy-relay-asks.json";
pub const ASK_VERSION: u32 = 1;
/// Records held at once; the oldest pushed is evicted.
pub const MAX_ASKS: usize = 512;
/// A desktop answer is remembered this long for a truthful phone ack.
pub const TOMBSTONE_TTL_MS: i64 = 15 * 60 * 1000;
pub const MAX_TOMBSTONES: usize = 32;
/// Shortest code; extended on collision up to `CODE_MAX_LEN`.
pub const CODE_MIN_LEN: usize = 4;
pub const CODE_MAX_LEN: usize = 7;

/// One published ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    pub code: String,
    /// The permission id (`per_...`).
    pub request: String,
    /// The session the ask belongs to (`ses_...`).
    pub session: String,
    /// Session title at push time.
    pub title: String,
    /// Epoch milliseconds.
    pub pushed: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct AskFile {
    version: u32,
    #[serde(default)]
    asks: Vec<Ask>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub code: String,
    /// The record dropped to stay within `MAX_ASKS`.
    pub evicted: Option<Ask>,
}

#[derive(Debug)]
pub enum RegisterError {
    /// `CODE_MAX_LEN` hex characters still collided; the push is skipped.
    CodeCollision {
        request: String,
    },
    Io(io::Error),
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CodeCollision { request } => {
                write!(f, "no free permission code for request {request}")
            }
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for RegisterError {}

/// Outcome of a reply token lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolve {
    /// No record and no tombstone.
    Empty,
    Found(Ask),
    /// A named code that is not registered. `available` is newest first.
    Unknown {
        requested: String,
        available: Vec<Ask>,
    },
    /// A record removed by a desktop answer, still inside the tombstone window.
    AlreadyAnswered(Ask),
}

/// What `open` had to do; the caller logs it, core never does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenReport {
    pub loaded: usize,
    pub trimmed: usize,
    pub quarantined: Option<PathBuf>,
    pub quarantine_error: Option<String>,
    pub read_error: Option<String>,
}

#[derive(Debug, Clone)]
struct Tombstone {
    code: String,
    request: String,
    title: String,
    at: i64,
}

struct Inner {
    asks: Vec<Ask>,
    tombstones: Vec<Tombstone>,
}

pub struct AskStore {
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl AskStore {
    /// Missing file is an empty registry; invalid JSON or a version other than
    /// 1 is quarantined and the registry starts empty. Startup is never
    /// blocked by the file.
    pub fn open(path: PathBuf, now_ms: i64) -> (Self, OpenReport) {
        let mut report = OpenReport::default();
        let raw = match fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                report.read_error = Some(error.to_string());
                None
            }
        };
        let asks = match raw {
            Some(text) => match serde_json::from_str::<AskFile>(&text) {
                Ok(file) if file.version == ASK_VERSION => file.asks,
                Ok(_) | Err(_) => {
                    report.quarantine_error = quarantine(&path, now_ms, &mut report);
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        report.loaded = asks.len();
        let (asks, trimmed) = normalize(asks);
        report.trimmed = trimmed;
        let store = Self {
            path,
            inner: Mutex::new(Inner {
                asks,
                tombstones: Vec::new(),
            }),
        };
        (store, report)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes the record before its push. A failed persist leaves memory
    /// unchanged so the next tick retries the same deterministic code.
    pub fn register(
        &self,
        request: &str,
        session: &str,
        title: &str,
        pushed: i64,
    ) -> Result<Registered, RegisterError> {
        let mut inner = self.lock();
        let mut next = inner.asks.clone();
        if let Some(index) = next.iter().position(|ask| ask.request == request) {
            let code = next[index].code.clone();
            if pushed <= next[index].pushed {
                return Ok(Registered {
                    code,
                    evicted: None,
                });
            }
            next[index].pushed = pushed;
            sort_asks(&mut next);
            persist(&self.path, &next).map_err(RegisterError::Io)?;
            inner.asks = next;
            return Ok(Registered {
                code,
                evicted: None,
            });
        }
        let code = allocate_code(&next, request)?;
        next.push(Ask {
            code: code.clone(),
            request: request.to_string(),
            session: session.to_string(),
            title: title.to_string(),
            pushed,
        });
        sort_asks(&mut next);
        let evicted = if next.len() > MAX_ASKS {
            Some(next.remove(0))
        } else {
            None
        };
        persist(&self.path, &next).map_err(RegisterError::Io)?;
        inner.asks = next;
        Ok(Registered { code, evicted })
    }

    pub fn resolve(&self, token: Option<&str>) -> Resolve {
        self.resolve_at(token, now_ms())
    }

    /// Deterministic form used by tests.
    pub fn resolve_at(&self, token: Option<&str>, now: i64) -> Resolve {
        let mut inner = self.lock();
        prune_tombstones(&mut inner.tombstones, now);
        let available = |asks: &[Ask]| {
            let mut newest = asks.to_vec();
            newest.sort_by(|left, right| {
                right
                    .pushed
                    .cmp(&left.pushed)
                    .then_with(|| left.request.cmp(&right.request))
            });
            newest
        };
        match token.map(normalize_code).filter(|code| !code.is_empty()) {
            Some(requested) => {
                if let Some(ask) = inner.asks.iter().find(|ask| ask.code == requested) {
                    return Resolve::Found(ask.clone());
                }
                if let Some(tombstone) = inner
                    .tombstones
                    .iter()
                    .find(|tombstone| tombstone.code == requested)
                {
                    return Resolve::AlreadyAnswered(tombstone_ask(tombstone));
                }
                if inner.asks.is_empty() {
                    return Resolve::Empty;
                }
                Resolve::Unknown {
                    requested,
                    available: available(&inner.asks),
                }
            }
            None => {
                if let Some(ask) = inner.asks.last() {
                    return Resolve::Found(ask.clone());
                }
                match inner.tombstones.last() {
                    Some(tombstone) => Resolve::AlreadyAnswered(tombstone_ask(tombstone)),
                    None => Resolve::Empty,
                }
            }
        }
    }

    /// Removes the record and keeps a tombstone for a later phone reply. The
    /// file is written before memory changes; a failed write keeps the record
    /// so a retry can map the server outcome to already answered.
    pub fn remove(&self, request: &str) -> Option<Ask> {
        let mut inner = self.lock();
        let index = inner.asks.iter().position(|ask| ask.request == request)?;
        let mut next = inner.asks.clone();
        let ask = next.remove(index);
        if persist(&self.path, &next).is_err() {
            return None;
        }
        inner.asks = next;
        inner.tombstones.push(Tombstone {
            code: ask.code.clone(),
            request: ask.request.clone(),
            title: ask.title.clone(),
            at: now_ms(),
        });
        if inner.tombstones.len() > MAX_TOMBSTONES {
            let drop = inner.tombstones.len() - MAX_TOMBSTONES;
            inner.tombstones.drain(..drop);
        }
        Some(ask)
    }

    /// Tombstone lookup by request id, pruned by the current time.
    pub fn answered(&self, request: &str) -> Option<Ask> {
        self.answered_at(request, now_ms())
    }

    /// Deterministic form used by tests.
    pub fn answered_at(&self, request: &str, now: i64) -> Option<Ask> {
        let mut inner = self.lock();
        prune_tombstones(&mut inner.tombstones, now);
        inner
            .tombstones
            .iter()
            .rev()
            .find(|tombstone| tombstone.request == request)
            .map(tombstone_ask)
    }

    pub fn count(&self) -> usize {
        self.lock().asks.len()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// Removes `<asks>.corrupt-<unix>` files older than `ttl_secs`.
pub fn prune_corrupt_backups(path: &Path, now_ms: i64, ttl_secs: i64) -> io::Result<usize> {
    crate::state::prune_corrupt_backups(path, now_ms, ttl_secs)
}

fn quarantine(path: &Path, now_ms: i64, report: &mut OpenReport) -> Option<String> {
    match crate::state::quarantine_state(path, now_ms) {
        Ok(target) => {
            report.quarantined = Some(target);
            None
        }
        Err(error) => Some(error.to_string()),
    }
}

fn normalize(mut asks: Vec<Ask>) -> (Vec<Ask>, usize) {
    let before = asks.len();
    sort_asks(&mut asks);
    asks.retain(|ask| !ask.code.is_empty() && !ask.request.is_empty() && !ask.session.is_empty());
    let mut codes = BTreeSet::new();
    let mut requests = BTreeSet::new();
    asks.reverse();
    asks.retain(|ask| codes.insert(ask.code.clone()) && requests.insert(ask.request.clone()));
    asks.reverse();
    if asks.len() > MAX_ASKS {
        let drop = asks.len() - MAX_ASKS;
        asks.drain(..drop);
    }
    let trimmed = before - asks.len();
    (asks, trimmed)
}

fn sort_asks(asks: &mut [Ask]) {
    asks.sort_by(|left, right| {
        left.pushed
            .cmp(&right.pushed)
            .then_with(|| left.request.cmp(&right.request))
    });
}

fn allocate_code(asks: &[Ask], request: &str) -> Result<String, RegisterError> {
    let digest = Sha256::digest(request.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    for len in CODE_MIN_LEN..=CODE_MAX_LEN {
        let candidate = &hex[..len];
        if !asks.iter().any(|ask| ask.code == candidate) {
            return Ok(candidate.to_string());
        }
    }
    Err(RegisterError::CodeCollision {
        request: request.to_string(),
    })
}

fn normalize_code(token: &str) -> String {
    let token = token.trim();
    let token = token.strip_prefix('#').unwrap_or(token);
    token.trim().to_ascii_lowercase()
}

fn prune_tombstones(tombstones: &mut Vec<Tombstone>, now: i64) {
    tombstones.retain(|tombstone| now.saturating_sub(tombstone.at) <= TOMBSTONE_TTL_MS);
}

fn tombstone_ask(tombstone: &Tombstone) -> Ask {
    Ask {
        code: tombstone.code.clone(),
        request: tombstone.request.clone(),
        session: String::new(),
        title: tombstone.title.clone(),
        pushed: tombstone.at,
    }
}

fn persist(path: &Path, asks: &[Ask]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let file = AskFile {
        version: ASK_VERSION,
        asks: asks.to_vec(),
    };
    let body = format!(
        "{}\n",
        serde_json::to_string(&file)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
    );
    let mut temp = OsString::from(path.as_os_str());
    temp.push(format!(".{}.tmp", std::process::id()));
    let temp = PathBuf::from(temp);
    let mut handle = crate::fsutil::owner_only(&temp)?;
    handle.write_all(body.as_bytes())?;
    handle.sync_all()?;
    drop(handle);
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    crate::fsutil::sync_parent(path);
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("core-asks-unit")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root.join(ASK_FILE)
    }

    #[test]
    fn the_newest_record_is_the_default_target() {
        let store = AskStore::open(temp_path("newest"), 0).0;
        store.register("per_1", "ses_1", "One", 100).unwrap();
        store.register("per_2", "ses_2", "Two", 200).unwrap();
        match store.resolve(None) {
            Resolve::Found(ask) => {
                assert_eq!(ask.request, "per_2");
                assert_eq!(ask.title, "Two");
            }
            other => panic!("expected the newest record, got {other:?}"),
        }
    }

    #[test]
    fn a_code_resolves_with_or_without_a_hash() {
        let store = AskStore::open(temp_path("code"), 0).0;
        let registered = store.register("per_1", "ses_1", "One", 100).unwrap();
        assert_eq!(registered.code.len(), CODE_MIN_LEN);
        for token in [
            registered.code.clone(),
            format!("#{}", registered.code),
            format!("#{}", registered.code.to_uppercase()),
        ] {
            match store.resolve(Some(&token)) {
                Resolve::Found(ask) => assert_eq!(ask.request, "per_1"),
                other => panic!("expected the record, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_code_is_deterministic() {
        let first = AskStore::open(temp_path("deterministic-a"), 0).0;
        let second = AskStore::open(temp_path("deterministic-b"), 0).0;
        let left = first.register("per_1", "ses_1", "One", 1).unwrap();
        let right = second.register("per_1", "ses_1", "One", 1).unwrap();
        assert_eq!(left.code, right.code);
    }

    #[test]
    fn re_registering_a_request_keeps_one_record() {
        let store = AskStore::open(temp_path("reregister"), 0).0;
        let first = store.register("per_1", "ses_1", "Old", 100).unwrap();
        let second = store.register("per_1", "ses_1", "Old", 50).unwrap();
        assert_eq!(first.code, second.code);
        assert_eq!(store.count(), 1);
        assert!(second.evicted.is_none());
    }

    #[test]
    fn the_cap_evicts_the_oldest_record() {
        let store = AskStore::open(temp_path("cap"), 0).0;
        for index in 0..MAX_ASKS {
            store
                .register(&format!("per_{index}"), "ses_1", "T", index as i64)
                .unwrap();
        }
        let registered = store
            .register("per_last", "ses_1", "T", MAX_ASKS as i64)
            .unwrap();
        let evicted = registered.evicted.expect("the oldest is evicted");
        assert_eq!(evicted.request, "per_0");
        assert_eq!(store.count(), MAX_ASKS);
    }

    #[test]
    fn an_unknown_code_lists_the_current_ones_newest_first() {
        let store = AskStore::open(temp_path("unknown"), 0).0;
        store.register("per_1", "ses_1", "One", 100).unwrap();
        store.register("per_2", "ses_2", "Two", 200).unwrap();
        match store.resolve(Some("ffff")) {
            Resolve::Unknown {
                requested,
                available,
            } => {
                assert_eq!(requested, "ffff");
                assert_eq!(available.len(), 2);
                assert_eq!(available[0].request, "per_2");
            }
            other => panic!("expected an unknown code, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_registry_has_no_target() {
        let store = AskStore::open(temp_path("empty"), 0).0;
        assert_eq!(store.resolve(None), Resolve::Empty);
        assert_eq!(store.resolve(Some("3f2a")), Resolve::Empty);
    }

    #[test]
    fn tombstones_cap_at_the_configured_length() {
        let store = AskStore::open(temp_path("tombstone-cap"), 0).0;
        for index in 0..(MAX_TOMBSTONES + 4) {
            let request = format!("per_{index}");
            store
                .register(&request, "ses_1", "T", index as i64)
                .unwrap();
            store.remove(&request).unwrap();
        }
        assert!(store.answered("per_0").is_none());
        assert!(store.answered("per_last").is_none());
        assert!(store
            .answered(&format!("per_{}", MAX_TOMBSTONES + 3))
            .is_some());
    }

    #[test]
    fn a_tombstone_expires_after_the_ttl() {
        let store = AskStore::open(temp_path("tombstone-ttl"), 0).0;
        store.register("per_1", "ses_1", "One", 1).unwrap();
        store.remove("per_1").unwrap();
        let now = now_ms();
        assert!(store.answered_at("per_1", now + TOMBSTONE_TTL_MS).is_some());
        assert!(store
            .answered_at("per_1", now + TOMBSTONE_TTL_MS + 1)
            .is_none());
    }
}
