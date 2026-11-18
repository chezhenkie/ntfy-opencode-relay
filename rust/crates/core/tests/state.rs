//! State file behavior, ported from the Node state tests in
//! `tests/ntfy-inbox-relay.test.mjs`.

use std::fs;
use std::path::{Path, PathBuf};

use ntfy_relay_core::state::{
    corrupt_state_path, prune_corrupt_backups, quarantine_state, read_state, read_state_report,
    write_state_atomic, State, StateError, CORRUPT_BACKUP_TTL_SECS, MAX_PROCESSED_IDS,
    MAX_TOPIC_ROWS,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("core-state-tests");
        let dir = base.join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn missing_file_is_empty_and_atomic_write_round_trips() {
    let temp = TempDir::new("roundtrip");
    let file = temp.path().join("state.json");
    assert_eq!(read_state(&file).unwrap(), State::new());

    let mut state = State::new();
    state.set_last("oc-x", "ses_1", 100);
    state.record_processed("oc-x", "m1", 100);
    write_state_atomic(&file, &state).unwrap();

    let loaded = read_state(&file).unwrap();
    assert_eq!(
        loaded.target("oc-x").unwrap().last.as_deref(),
        Some("ses_1")
    );
    assert_eq!(loaded.topics["oc-x"].last_time, 100);
    assert_eq!(loaded.topics["oc-x"].last_ids, vec!["m1"]);

    let names: Vec<String> = fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["state.json"]);
}

#[test]
fn corrupt_json_is_invalid() {
    let temp = TempDir::new("invalid");
    let file = temp.path().join("state.json");
    fs::write(&file, "{nope").unwrap();
    match read_state(&file) {
        Err(StateError::Invalid(error)) => {
            assert!(error.to_string().contains("key") || error.to_string().contains("expected"));
        }
        other => panic!("expected invalid JSON, got {other:?}"),
    }
}

#[test]
fn writes_are_compact_json() {
    let temp = TempDir::new("compact");
    let file = temp.path().join("state.json");
    let mut state = State::new();
    state.record_processed("oc-x", "m1", 100);
    write_state_atomic(&file, &state).unwrap();
    let raw = fs::read_to_string(&file).unwrap();
    assert_eq!(raw.lines().count(), 1);
    assert!(raw.ends_with('\n'));
    assert!(!raw.contains("\n  "));
}

#[test]
fn corrupt_file_is_quarantined() {
    let temp = TempDir::new("quarantine");
    let file = temp.path().join("state.json");
    fs::write(&file, "{nope").unwrap();
    let target = quarantine_state(&file, 1_700_000_000_000).unwrap();
    assert_eq!(target, corrupt_state_path(&file, 1_700_000_000_000));
    assert!(target.exists());
    assert!(!file.exists());
}

#[test]
fn prune_keeps_fresh_corrupt_backups() {
    let temp = TempDir::new("prune");
    let file = temp.path().join("state.json");
    fs::write(&file, "{}").unwrap();
    let now_ms: i64 = 1_700_000_000_000;
    let now_secs = now_ms.div_euclid(1000);
    let old = temp.path().join("state.json.corrupt-1600000000");
    let fresh = temp
        .path()
        .join(format!("state.json.corrupt-{}", now_secs - 60));
    let unrelated = temp.path().join("state.json.corrupt-nope");
    for path in [&old, &fresh, &unrelated] {
        fs::write(path, "{}").unwrap();
    }
    let removed = prune_corrupt_backups(&file, now_ms, CORRUPT_BACKUP_TTL_SECS).unwrap();
    assert_eq!(removed, 1);
    assert!(!old.exists());
    assert!(fresh.exists());
    assert!(unrelated.exists());
}

#[test]
fn prune_tolerates_a_missing_directory() {
    let temp = TempDir::new("prune-missing");
    let file = temp.path().join("nested").join("state.json");
    assert_eq!(prune_corrupt_backups(&file, 0, 0).unwrap(), 0);
}

#[test]
fn restart_cursor_keeps_the_newest_window() {
    let mut state = State::new();
    for index in 0..600 {
        state.record_processed("oc-x", &format!("m{index}"), 100 + index);
    }
    let text = serde_json::to_string(&state).unwrap();
    let loaded: State = serde_json::from_str(&text).unwrap();
    let topic = &loaded.topics["oc-x"];
    assert_eq!(topic.last_ids.len(), MAX_PROCESSED_IDS);
    assert_eq!(topic.last_ids[0], "m100");
    assert_eq!(topic.last_time, 100 + 599);
}

#[test]
fn an_unknown_version_is_rejected_for_quarantine() {
    let temp = TempDir::new("version");
    let file = temp.path().join("state.json");
    fs::write(&file, r#"{"version":2,"targets":{},"topics":{}}"#).unwrap();
    match read_state(&file) {
        Err(StateError::Version(2)) => {}
        other => panic!("expected a version error, got {other:?}"),
    }
}

#[test]
fn a_hand_edited_file_loads_bounded_and_reports_the_trim() {
    let temp = TempDir::new("bounded");
    let file = temp.path().join("state.json");
    let mut topics = String::new();
    let ids: Vec<String> = (0..10_000).map(|index| format!("\"m{index}\"")).collect();
    topics.push_str(&format!(
        "\"oc-999\":{{\"lastTime\":100000,\"lastIds\":[{}]}}",
        ids.join(",")
    ));
    for index in 0..600 {
        topics.push_str(&format!(
            ",\"oc-{index:03}\":{{\"lastTime\":{},\"lastIds\":[\"m{index}\"]}}",
            1_000 + index
        ));
    }
    fs::write(
        &file,
        format!(r#"{{"version":1,"targets":{{}},"topics":{{{topics}}}}}"#),
    )
    .unwrap();

    let (state, report) = read_state_report(&file).unwrap();
    assert_eq!(state.topics.len(), MAX_TOPIC_ROWS);
    assert_eq!(report.dropped_topics, 601 - MAX_TOPIC_ROWS);
    assert_eq!(report.truncated_ids, 10_000 - MAX_PROCESSED_IDS);
    let newest = &state.topics["oc-999"];
    assert_eq!(newest.last_ids.len(), MAX_PROCESSED_IDS);
    assert_eq!(newest.last_ids[0], "m9500");
    assert!(!state.topics.contains_key("oc-088"));
}

#[cfg(unix)]
#[test]
fn state_and_temp_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new("mode");
    // A Windows mount in WSL reports 0777 for every file and ignores chmod;
    // the mode contract is only observable on a Unix filesystem.
    let probe = temp.path().join("probe");
    fs::write(&probe, b"x").unwrap();
    if fs::metadata(&probe).unwrap().permissions().mode() & 0o022 != 0 {
        return;
    }
    let file = temp.path().join("state.json");
    let mut state = State::new();
    state.record_processed("oc-x", "m1", 100);
    write_state_atomic(&file, &state).unwrap();
    let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}
