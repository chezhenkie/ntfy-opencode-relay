//! Ask registry file behavior: durability, quarantine, and code extension.

use std::fs;
use std::path::{Path, PathBuf};

use ntfy_relay_core::asks::{
    AskStore, OpenReport, RegisterError, Resolve, ASK_FILE, ASK_VERSION, MAX_ASKS, MAX_TOMBSTONES,
};
use sha2::{Digest, Sha256};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("core-asks-tests");
        let dir = base.join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join(ASK_FILE)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn hash(request: &str) -> String {
    let digest = Sha256::digest(request.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn records_round_trip_through_the_file() {
    let temp = TempDir::new("roundtrip");
    let (store, report) = AskStore::open(temp.file(), 1_700_000_000_000);
    assert_eq!(report, OpenReport::default());
    let first = store.register("per_1", "ses_1", "Fix links", 100).unwrap();
    store.register("per_2", "ses_2", "Other", 200).unwrap();

    let (loaded, report) = AskStore::open(temp.file(), 1_700_000_000_000);
    assert_eq!(report.loaded, 2);
    assert_eq!(report.trimmed, 0);
    assert_eq!(loaded.count(), 2);
    match loaded.resolve(Some(&first.code)) {
        Resolve::Found(ask) => {
            assert_eq!(ask.request, "per_1");
            assert_eq!(ask.session, "ses_1");
            assert_eq!(ask.title, "Fix links");
        }
        other => panic!("expected the persisted record, got {other:?}"),
    }
    let raw = fs::read_to_string(temp.file()).unwrap();
    assert_eq!(raw.lines().count(), 1);
    assert!(raw.ends_with('\n'));
    assert!(raw.contains(&format!(r#""version":{ASK_VERSION}"#)));
    assert!(!raw.contains("\n  "));
}

#[test]
fn a_large_file_is_trimmed_to_the_cap() {
    let temp = TempDir::new("trim");
    let mut rows = Vec::new();
    for index in 0..(MAX_ASKS + 3) {
        rows.push(format!(
            r#"{{"code":"c{index:04}","request":"per_{index}","session":"ses_1","title":"T","pushed":{index}}}"#
        ));
    }
    fs::write(
        temp.file(),
        format!(r#"{{"version":1,"asks":[{}]}}"#, rows.join(",")),
    )
    .unwrap();
    let (store, report) = AskStore::open(temp.file(), 0);
    assert_eq!(report.loaded, MAX_ASKS + 3);
    assert_eq!(report.trimmed, 3);
    assert_eq!(store.count(), MAX_ASKS);
}

#[test]
fn corrupt_json_is_quarantined_and_the_registry_starts_empty() {
    let temp = TempDir::new("corrupt");
    fs::write(temp.file(), "{nope").unwrap();
    let (store, report) = AskStore::open(temp.file(), 1_700_000_000_000);
    assert_eq!(store.count(), 0);
    assert!(report.quarantined.is_some());
    assert!(!temp.file().exists());
    let quarantined = report.quarantined.unwrap();
    assert!(fs::read_to_string(&quarantined).unwrap().contains("nope"));
}

#[test]
fn an_unknown_version_is_quarantined() {
    let temp = TempDir::new("version");
    fs::write(temp.file(), r#"{"version":2,"asks":[]}"#).unwrap();
    let (store, report) = AskStore::open(temp.file(), 1_700_000_000_000);
    assert_eq!(store.count(), 0);
    assert!(report.quarantined.is_some());
}

#[test]
fn a_code_extends_to_seven_characters_on_collision() {
    let temp = TempDir::new("collision");
    let request = "per_target";
    let hash = hash(request);
    let rows = [
        (hash[..4].to_string(), "per_a"),
        (hash[..5].to_string(), "per_b"),
        (hash[..6].to_string(), "per_c"),
    ]
    .iter()
    .map(|(code, other)| {
        format!(
            r#"{{"code":"{code}","request":"{other}","session":"ses_1","title":"T","pushed":1}}"#
        )
    })
    .collect::<Vec<_>>();
    fs::write(
        temp.file(),
        format!(r#"{{"version":1,"asks":[{}]}}"#, rows.join(",")),
    )
    .unwrap();

    let (store, _) = AskStore::open(temp.file(), 0);
    let registered = store.register(request, "ses_9", "Target", 2).unwrap();
    assert_eq!(registered.code.len(), 7);
    assert_eq!(registered.code, hash[..7]);
}

#[test]
fn a_seventh_collision_skips_the_push() {
    let temp = TempDir::new("collision-full");
    let request = "per_target";
    let hash = hash(request);
    let rows = [4usize, 5, 6, 7]
        .iter()
        .enumerate()
        .map(|(index, len)| {
            format!(
                r#"{{"code":"{}","request":"per_{index}","session":"ses_1","title":"T","pushed":1}}"#,
                &hash[..*len]
            )
        })
        .collect::<Vec<_>>();
    fs::write(
        temp.file(),
        format!(r#"{{"version":1,"asks":[{}]}}"#, rows.join(",")),
    )
    .unwrap();
    let (store, _) = AskStore::open(temp.file(), 0);
    match store.register(request, "ses_9", "Target", 2) {
        Err(RegisterError::CodeCollision { request: failed }) => assert_eq!(failed, request),
        other => panic!("expected a code collision, got {other:?}"),
    }
    assert_eq!(store.count(), 4);
}

#[test]
fn a_duplicate_code_in_a_hand_edited_file_keeps_the_newest() {
    let temp = TempDir::new("duplicate");
    fs::write(
        temp.file(),
        r#"{"version":1,"asks":[
            {"code":"abcd","request":"per_1","session":"ses_1","title":"Old","pushed":1},
            {"code":"abcd","request":"per_2","session":"ses_2","title":"New","pushed":2}
        ]}"#,
    )
    .unwrap();
    let (store, report) = AskStore::open(temp.file(), 0);
    assert_eq!(report.trimmed, 1);
    assert_eq!(store.count(), 1);
    match store.resolve(None) {
        Resolve::Found(ask) => {
            assert_eq!(ask.request, "per_2");
            assert_eq!(ask.title, "New");
        }
        other => panic!("expected the newest duplicate, got {other:?}"),
    }
}

#[test]
fn a_desktop_answer_moves_the_record_to_a_tombstone() {
    let temp = TempDir::new("tombstone");
    let (store, _) = AskStore::open(temp.file(), 0);
    store.register("per_1", "ses_1", "Fix links", 100).unwrap();
    let removed = store.remove("per_1").expect("the record exists");
    assert_eq!(removed.code.len(), 4);
    assert_eq!(store.count(), 0);
    match store.resolve(None) {
        Resolve::AlreadyAnswered(ask) => {
            assert_eq!(ask.request, "per_1");
            assert_eq!(ask.title, "Fix links");
        }
        other => panic!("expected an already answered record, got {other:?}"),
    }
    assert!(store.answered("per_1").is_some());
    let (reopened, report) = AskStore::open(temp.file(), 0);
    assert_eq!(report.loaded, 0);
    assert_eq!(reopened.count(), 0);
}

#[test]
fn removing_an_unknown_request_returns_none() {
    let temp = TempDir::new("remove-missing");
    let (store, _) = AskStore::open(temp.file(), 0);
    assert!(store.remove("per_none").is_none());
}

#[test]
fn the_tombstone_list_stays_bounded() {
    let temp = TempDir::new("tombstone-bound");
    let (store, _) = AskStore::open(temp.file(), 0);
    for index in 0..(MAX_TOMBSTONES + 2) {
        let request = format!("per_{index}");
        store
            .register(&request, "ses_1", "T", index as i64)
            .unwrap();
        store.remove(&request).unwrap();
    }
    assert!(store.answered("per_0").is_none());
    assert!(store
        .answered(&format!("per_{}", MAX_TOMBSTONES + 1))
        .is_some());
}

#[cfg(unix)]
#[test]
fn the_registry_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new("mode");
    let probe = temp.0.join("probe");
    fs::write(&probe, b"x").unwrap();
    if fs::metadata(&probe).unwrap().permissions().mode() & 0o022 != 0 {
        return;
    }
    let (store, _) = AskStore::open(temp.file(), 0);
    store.register("per_1", "ses_1", "T", 1).unwrap();
    let mode = fs::metadata(temp.file()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}
