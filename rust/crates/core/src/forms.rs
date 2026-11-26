//! Registry of question forms that reached the phone.
//!
//! A record is written before its push is published, so a phone reply finds
//! the form that was seen instead of a routed session. Records are bound to the
//! OpenCode form id and session id; the short code is a prefix of the form id's
//! sha256 digest, extended only on collision, the same rule as the ask
//! registry. The code is an internal identity and a tombstone key, never a
//! phone instruction.
//!
//! The registry does not interpret answers. `/answer` cancels the newest record
//! with the user's free text; the assistant decides whether it is enough.
//! Hidden fields are not registered.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fsutil::BoundedRead;

pub use crate::asks::OpenReport;

/// File name next to the relay state file.
pub const FORM_FILE: &str = "ntfy-relay-forms.json";
pub const FORM_VERSION: u32 = 1;
/// Pending forms held at once; the oldest pushed is evicted.
pub const MAX_FORMS: usize = 64;
/// Hard read cap; a larger file is quarantined instead of parsed.
pub const MAX_FORM_BYTES: usize = 512 * 1024;
/// A form answered on the desktop is remembered this long for a phone ack.
pub const TOMBSTONE_TTL_MS: i64 = 15 * 60 * 1000;
pub const MAX_TOMBSTONES: usize = 16;
pub const CODE_MIN_LEN: usize = 4;
pub const CODE_MAX_LEN: usize = 7;

/// One option as the push rendered it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormOption {
    pub label: String,
    /// The value the form expects, which can differ from the label.
    pub value: String,
}

/// One answerable field of a form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormField {
    pub key: String,
    pub title: String,
    /// `string`, `number`, `integer`, `boolean`, or `multiselect`.
    pub kind: String,
    pub required: bool,
    /// Several values are allowed; multiselect fields always set it.
    pub multiple: bool,
    pub options: Vec<FormOption>,
}

/// One pushed form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormRecord {
    pub code: String,
    /// The form id (`frm_...`).
    pub form: String,
    pub session: String,
    pub topic: String,
    pub title: String,
    /// Epoch milliseconds.
    pub pushed: i64,
    /// Visible fields only, in push order.
    pub fields: Vec<FormField>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FormFile {
    version: u32,
    #[serde(default)]
    forms: Vec<FormRecord>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Registered {
    pub code: String,
    /// The record dropped to stay within `MAX_FORMS`.
    pub evicted: Option<FormRecord>,
}

#[derive(Debug)]
pub enum RegisterError {
    CodeCollision { form: String },
    Io(io::Error),
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CodeCollision { form } => write!(f, "no free form code for {form}"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for RegisterError {}

#[derive(Debug, Clone, PartialEq)]
pub enum Resolve {
    Empty,
    Found(FormRecord),
    Unknown {
        requested: String,
        available: Vec<FormRecord>,
    },
    /// Removed by a desktop answer or a submitted reply inside the window.
    AlreadyAnswered(FormRecord),
}

#[derive(Debug, Clone)]
struct Tombstone {
    code: String,
    form: String,
    topic: String,
    title: String,
    at: i64,
}

struct Inner {
    forms: Vec<FormRecord>,
    tombstones: Vec<Tombstone>,
}

pub struct FormStore {
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl FormStore {
    /// Missing file is an empty registry; a file that is unreadable, oversized,
    /// not UTF-8, invalid JSON, or another version is quarantined and the
    /// registry starts empty. Startup is never blocked.
    pub fn open(path: PathBuf, now_ms: i64) -> (Self, OpenReport) {
        let mut report = OpenReport::default();
        let raw = match crate::fsutil::read_bounded(&path, MAX_FORM_BYTES) {
            Ok(BoundedRead::Missing) => None,
            Ok(BoundedRead::Text(text)) => Some(text),
            Ok(BoundedRead::Oversize) => {
                report.read_error = Some(format!("file is larger than {MAX_FORM_BYTES} bytes"));
                report.quarantine_error = quarantine(&path, now_ms, &mut report);
                None
            }
            Ok(BoundedRead::NotText) => {
                report.read_error = Some("file is not valid UTF-8".to_string());
                report.quarantine_error = quarantine(&path, now_ms, &mut report);
                None
            }
            Err(error) => {
                report.read_error = Some(error.to_string());
                report.quarantine_error = quarantine(&path, now_ms, &mut report);
                None
            }
        };
        let forms = match raw {
            Some(text) => match serde_json::from_str::<FormFile>(&text) {
                Ok(file) if file.version == FORM_VERSION => file.forms,
                Ok(_) | Err(_) => {
                    report.quarantine_error = quarantine(&path, now_ms, &mut report);
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        report.loaded = forms.len();
        let (forms, trimmed) = normalize(forms);
        report.trimmed = trimmed;
        let store = Self {
            path,
            inner: Mutex::new(Inner {
                forms,
                tombstones: Vec::new(),
            }),
        };
        (store, report)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes the record before its push. A failed persist leaves memory
    /// unchanged so the next tick retries the same deterministic code. An
    /// existing record keeps its code and refreshes its push time.
    pub fn register(&self, mut record: FormRecord) -> Result<Registered, RegisterError> {
        let mut inner = self.lock();
        let mut next = inner.forms.clone();
        if let Some(index) = next.iter().position(|form| form.form == record.form) {
            let code = next[index].code.clone();
            if record.pushed <= next[index].pushed {
                return Ok(Registered {
                    code,
                    evicted: None,
                });
            }
            record.code = code.clone();
            next[index] = record;
            sort_forms(&mut next);
            persist(&self.path, &next).map_err(RegisterError::Io)?;
            inner.forms = next;
            return Ok(Registered {
                code,
                evicted: None,
            });
        }
        let code = allocate_code(&next, &inner.tombstones, &record.form)?;
        record.code = code.clone();
        next.push(record);
        sort_forms(&mut next);
        let evicted = if next.len() > MAX_FORMS {
            Some(next.remove(0))
        } else {
            None
        };
        persist(&self.path, &next).map_err(RegisterError::Io)?;
        inner.forms = next;
        Ok(Registered { code, evicted })
    }

    pub fn resolve(&self, code: Option<&str>, topic: &str) -> Resolve {
        self.resolve_at(code, topic, now_ms())
    }

    /// Deterministic form used by tests. A record written before the topic
    /// field existed carries an empty topic and answers on any topic.
    pub fn resolve_at(&self, code: Option<&str>, topic: &str, now: i64) -> Resolve {
        let mut inner = self.lock();
        prune_tombstones(&mut inner.tombstones, now);
        let same_topic = |held: &str| topic.is_empty() || held.is_empty() || held == topic;
        let available = |forms: Vec<&FormRecord>| {
            let mut newest: Vec<FormRecord> = forms.into_iter().cloned().collect();
            newest.sort_by(|left, right| {
                right
                    .pushed
                    .cmp(&left.pushed)
                    .then_with(|| left.form.cmp(&right.form))
            });
            newest
        };
        let requested = code.map(normalize_code).filter(|code| !code.is_empty());
        let Some(requested) = requested else {
            // No code: the newest pushed form on this topic.
            if let Some(form) = inner
                .forms
                .iter()
                .filter(|form| same_topic(&form.topic))
                .max_by(|left, right| {
                    left.pushed
                        .cmp(&right.pushed)
                        .then_with(|| right.form.cmp(&left.form))
                })
            {
                return Resolve::Found(form.clone());
            }
            return match inner
                .tombstones
                .iter()
                .rev()
                .find(|tombstone| same_topic(&tombstone.topic))
            {
                Some(tombstone) => Resolve::AlreadyAnswered(tombstone_form(tombstone)),
                None => Resolve::Empty,
            };
        };
        if let Some(form) = inner
            .forms
            .iter()
            .find(|form| form.code == requested && same_topic(&form.topic))
        {
            return Resolve::Found(form.clone());
        }
        if let Some(tombstone) = inner
            .tombstones
            .iter()
            .find(|tombstone| tombstone.code == requested && same_topic(&tombstone.topic))
        {
            return Resolve::AlreadyAnswered(tombstone_form(tombstone));
        }
        let matching: Vec<&FormRecord> = inner
            .forms
            .iter()
            .filter(|form| same_topic(&form.topic))
            .collect();
        if matching.is_empty() {
            return Resolve::Empty;
        }
        Resolve::Unknown {
            requested,
            available: available(matching),
        }
    }

    /// Removes the record and keeps a tombstone for a later phone reply.
    pub fn remove(&self, form: &str) -> Option<FormRecord> {
        let mut inner = self.lock();
        let index = inner.forms.iter().position(|record| record.form == form)?;
        let mut next = inner.forms.clone();
        let record = next.remove(index);
        if persist(&self.path, &next).is_err() {
            return None;
        }
        inner.forms = next;
        inner.tombstones.push(Tombstone {
            code: record.code.clone(),
            form: record.form.clone(),
            topic: record.topic.clone(),
            title: record.title.clone(),
            at: now_ms(),
        });
        if inner.tombstones.len() > MAX_TOMBSTONES {
            let drop = inner.tombstones.len() - MAX_TOMBSTONES;
            inner.tombstones.drain(..drop);
        }
        Some(record)
    }

    /// Tombstone lookup by form id, pruned by the current time.
    pub fn answered(&self, form: &str) -> Option<FormRecord> {
        self.answered_at(form, now_ms())
    }

    /// Deterministic form used by tests.
    pub fn answered_at(&self, form: &str, now: i64) -> Option<FormRecord> {
        let mut inner = self.lock();
        prune_tombstones(&mut inner.tombstones, now);
        inner
            .tombstones
            .iter()
            .rev()
            .find(|tombstone| tombstone.form == form)
            .map(tombstone_form)
    }

    pub fn count(&self) -> usize {
        self.lock().forms.len()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// Removes `<forms>.corrupt-<unix>` files older than `ttl_secs`.
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

fn normalize(mut forms: Vec<FormRecord>) -> (Vec<FormRecord>, usize) {
    let before = forms.len();
    sort_forms(&mut forms);
    forms.retain(|form| !form.code.is_empty() && !form.form.is_empty() && !form.session.is_empty());
    let mut codes = BTreeSet::new();
    let mut ids = BTreeSet::new();
    forms.reverse();
    forms.retain(|form| codes.insert(form.code.clone()) && ids.insert(form.form.clone()));
    forms.reverse();
    if forms.len() > MAX_FORMS {
        let drop = forms.len() - MAX_FORMS;
        forms.drain(..drop);
    }
    let trimmed = before - forms.len();
    (forms, trimmed)
}

fn sort_forms(forms: &mut [FormRecord]) {
    forms.sort_by(|left, right| {
        left.pushed
            .cmp(&right.pushed)
            .then_with(|| left.form.cmp(&right.form))
    });
}

fn allocate_code(
    forms: &[FormRecord],
    tombstones: &[Tombstone],
    form: &str,
) -> Result<String, RegisterError> {
    let digest = Sha256::digest(form.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    for len in CODE_MIN_LEN..=CODE_MAX_LEN {
        let candidate = &hex[..len];
        if !forms.iter().any(|record| record.code == candidate)
            && !tombstones
                .iter()
                .any(|tombstone| tombstone.code == candidate)
        {
            return Ok(candidate.to_string());
        }
    }
    Err(RegisterError::CodeCollision {
        form: form.to_string(),
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

fn tombstone_form(tombstone: &Tombstone) -> FormRecord {
    FormRecord {
        code: tombstone.code.clone(),
        form: tombstone.form.clone(),
        session: String::new(),
        topic: tombstone.topic.clone(),
        title: tombstone.title.clone(),
        pushed: tombstone.at,
        fields: Vec::new(),
    }
}

fn persist(path: &Path, forms: &[FormRecord]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let file = FormFile {
        version: FORM_VERSION,
        forms: forms.to_vec(),
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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
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
            .join("core-forms-unit")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root.join(FORM_FILE)
    }

    fn field(key: &str, kind: &str, required: bool) -> FormField {
        FormField {
            key: key.to_string(),
            title: key.to_string(),
            kind: kind.to_string(),
            required,
            multiple: kind == "multiselect",
            options: Vec::new(),
        }
    }

    fn record(form: &str, pushed: i64, fields: Vec<FormField>) -> FormRecord {
        FormRecord {
            code: String::new(),
            form: form.to_string(),
            session: "ses_1".to_string(),
            topic: "proj".to_string(),
            title: "Questions".to_string(),
            pushed,
            fields,
        }
    }

    #[test]
    fn a_registered_form_gets_a_short_deterministic_code() {
        let first = FormStore::open(temp_path("code-a"), 0).0;
        let second = FormStore::open(temp_path("code-b"), 0).0;
        let left = first
            .register(record("frm_1", 1, vec![field("note", "string", false)]))
            .unwrap();
        let right = second
            .register(record("frm_1", 1, vec![field("note", "string", false)]))
            .unwrap();
        assert_eq!(left.code, right.code);
        assert_eq!(left.code.len(), CODE_MIN_LEN);
        assert!(matches!(
            first.resolve(Some(&left.code), "proj"),
            Resolve::Found(_)
        ));
    }

    #[test]
    fn a_repeated_push_refreshes_one_record() {
        let store = FormStore::open(temp_path("repeat"), 0).0;
        let first = store
            .register(record("frm_1", 10, vec![field("a", "string", true)]))
            .unwrap();
        let again = store
            .register(record("frm_1", 20, vec![field("a", "string", true)]))
            .unwrap();
        assert_eq!(again.code, first.code);
        let Resolve::Found(form) = store.resolve(Some(&again.code), "proj") else {
            panic!("the record survives");
        };
        assert_eq!(form.pushed, 20);
        assert_eq!(store.count(), 1);
    }

    #[test]
    fn the_cap_evicts_the_oldest_form() {
        let store = FormStore::open(temp_path("cap"), 0).0;
        for index in 0..MAX_FORMS {
            store
                .register(record(
                    &format!("frm_{index}"),
                    index as i64,
                    vec![field("note", "string", false)],
                ))
                .unwrap();
        }
        let registered = store
            .register(record(
                "frm_last",
                MAX_FORMS as i64,
                vec![field("note", "string", false)],
            ))
            .unwrap();
        assert_eq!(registered.evicted.unwrap().form, "frm_0");
        assert_eq!(store.count(), MAX_FORMS);
    }

    #[test]
    fn an_unknown_code_lists_the_pending_forms_newest_first() {
        let store = FormStore::open(temp_path("unknown"), 0).0;
        store
            .register(record("frm_1", 100, vec![field("a", "string", false)]))
            .unwrap();
        store
            .register(record("frm_2", 200, vec![field("a", "string", false)]))
            .unwrap();
        match store.resolve(Some("ffff"), "proj") {
            Resolve::Unknown {
                requested,
                available,
            } => {
                assert_eq!(requested, "ffff");
                assert_eq!(available.len(), 2);
                assert_eq!(available[0].form, "frm_2");
            }
            other => panic!("expected an unknown code, got {other:?}"),
        }
        assert!(
            matches!(store.resolve(None, "proj"), Resolve::Found(form) if form.form == "frm_2")
        );
    }

    #[test]
    fn a_removed_form_answers_from_the_tombstone() {
        let store = FormStore::open(temp_path("tombstone"), 0).0;
        let registered = store
            .register(record("frm_1", 1, vec![field("note", "string", false)]))
            .unwrap();
        store.remove("frm_1").unwrap();
        match store.resolve(Some(&registered.code), "proj") {
            Resolve::AlreadyAnswered(form) => assert_eq!(form.form, "frm_1"),
            other => panic!("expected a tombstone, got {other:?}"),
        }
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn a_non_text_file_is_quarantined_and_left_readable() {
        let path = temp_path("not-text");
        fs::write(&path, [0x7b, 0xff, 0x7d]).unwrap();
        let (store, report) = FormStore::open(path.clone(), 1_700_000_000_000);
        assert_eq!(store.count(), 0);
        assert!(report.read_error.is_some());
        let quarantined = report.quarantined.expect("the sample is kept");
        assert_eq!(fs::read(&quarantined).unwrap(), [0x7b, 0xff, 0x7d]);
        assert!(!path.exists());
    }

    #[test]
    fn a_tombstoned_code_is_not_reissued() {
        let store = FormStore::open(temp_path("tombstone-code"), 0).0;
        let removed = store
            .register(record("frm_old", 1, vec![field("a", "string", false)]))
            .unwrap();
        store.remove("frm_old").unwrap();
        let prefix = removed.code.clone();
        let mut index = 0u32;
        let form = loop {
            let text = format!("frm_new_{index}");
            let digest = Sha256::digest(text.as_bytes());
            let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            if hex.starts_with(&prefix) {
                break text;
            }
            index += 1;
        };
        let registered = store
            .register(record(&form, 2, vec![field("a", "string", false)]))
            .unwrap();
        assert!(
            registered.code.len() > prefix.len(),
            "the live tombstone must push the code past {prefix}"
        );
        assert!(matches!(
            store.resolve(Some(&prefix), "proj"),
            Resolve::AlreadyAnswered(_)
        ));
    }

    #[test]
    fn tombstones_cap_and_expire() {
        let store = FormStore::open(temp_path("tombstone-cap"), 0).0;
        for index in 0..(MAX_TOMBSTONES + 4) {
            let form = format!("frm_{index}");
            store
                .register(record(
                    &form,
                    index as i64,
                    vec![field("a", "string", false)],
                ))
                .unwrap();
            store.remove(&form).unwrap();
        }
        assert!(store.answered("frm_0").is_none());
        let last = format!("frm_{}", MAX_TOMBSTONES + 3);
        assert!(store.answered(&last).is_some());
        let stamp = now_ms();
        assert!(store.answered_at(&last, stamp + TOMBSTONE_TTL_MS).is_some());
        assert!(store
            .answered_at(&last, stamp + TOMBSTONE_TTL_MS + 1)
            .is_none());
    }
}
