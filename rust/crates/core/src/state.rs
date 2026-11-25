//! Relay state: the routing hints per topic plus the replay cursor per topic.
//!
//! Hints are advisory. `last` records the session that accepted the previous
//! prompt and is only honored inside a short continuity window; `pending` is a
//! one-shot target from `/new` or `/to`. Neither is routing authority: the
//! resolver validates both against a live session snapshot on every message,
//! and an old hint simply expires. There is no pin.
//!
//! Mirrors the state half of `ntfy-inbox-relay.mjs`: version 1, compact JSON,
//! an atomic temp-file rename, a 500-id window per topic, and quarantine for a
//! file that is not valid JSON. The file is owned by the daemon; readers see
//! either the old or the new file.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const STATE_VERSION: u32 = 1;
pub const MAX_PROCESSED_IDS: usize = 500;
pub const MAX_TOPIC_ROWS: usize = 512;
/// Quarantined state files older than this are removed at startup.
pub const CORRUPT_BACKUP_TTL_SECS: i64 = 7 * 24 * 60 * 60;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopicState {
    #[serde(default)]
    pub last_time: i64,
    #[serde(default)]
    pub last_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetState {
    /// Session that accepted the previous prompt on this topic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    #[serde(default)]
    pub last_at: i64,
    /// One-shot target from `/new` or `/to`; consumed by the next prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
    #[serde(default)]
    pub pending_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub targets: BTreeMap<String, TargetState>,
    #[serde(default)]
    pub topics: BTreeMap<String, TopicState>,
}

fn default_version() -> u32 {
    STATE_VERSION
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            version: STATE_VERSION,
            targets: BTreeMap::new(),
            topics: BTreeMap::new(),
        }
    }

    pub fn target(&self, topic: &str) -> Option<&TargetState> {
        self.targets.get(topic)
    }

    fn target_mut(&mut self, topic: &str) -> &mut TargetState {
        self.targets.entry(topic.to_string()).or_default()
    }

    pub fn set_last(&mut self, topic: &str, session_id: &str, at: i64) {
        let target = self.target_mut(topic);
        target.last = Some(session_id.to_string());
        target.last_at = at;
    }

    /// Set or clear the one-shot pending target. Clearing keeps `last`.
    pub fn set_pending(&mut self, topic: &str, session_id: Option<&str>, at: i64) {
        let target = self.target_mut(topic);
        target.pending = session_id.map(str::to_string);
        target.pending_at = if session_id.is_some() { at } else { 0 };
        if target.last.is_none() && target.pending.is_none() {
            self.targets.remove(topic);
        }
    }

    pub fn topic(&mut self, topic: &str) -> &mut TopicState {
        self.topics.entry(topic.to_string()).or_default()
    }

    /// Record an accepted message id and advance the topic cursor. The id
    /// window keeps the newest `cap` entries.
    pub fn record_processed(&mut self, topic: &str, id: &str, time: i64) {
        self.record_processed_with_cap(topic, id, time, MAX_PROCESSED_IDS);
    }

    pub fn record_processed_with_cap(&mut self, topic: &str, id: &str, time: i64, cap: usize) {
        let entry = self.topic(topic);
        if time > entry.last_time {
            entry.last_time = time;
        }
        entry.last_ids.push(id.to_string());
        if entry.last_ids.len() > cap {
            let drop = entry.last_ids.len() - cap;
            entry.last_ids.drain(..drop);
        }
    }
}

#[derive(Debug)]
pub enum StateError {
    Io(io::Error),
    Invalid(serde_json::Error),
    Version(u32),
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Invalid(error) => write!(f, "invalid JSON: {error}"),
            Self::Version(version) => write!(f, "unsupported state version {version}"),
        }
    }
}

impl std::error::Error for StateError {}

/// What the load had to drop to keep the file bounded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoadReport {
    pub truncated_ids: usize,
    pub dropped_topics: usize,
}

/// Missing file means a fresh state. Any other read or version error is
/// surfaced so the caller can quarantine the file.
pub fn read_state(path: &Path) -> Result<State, StateError> {
    read_state_report(path).map(|(state, _)| state)
}

/// `read_state` plus the normalization counts for one log line.
pub fn read_state_report(path: &Path) -> Result<(State, LoadReport), StateError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((State::new(), LoadReport::default()))
        }
        Err(error) => return Err(StateError::Io(error)),
    };
    let mut state: State = serde_json::from_str(&text).map_err(StateError::Invalid)?;
    if state.version != STATE_VERSION {
        return Err(StateError::Version(state.version));
    }
    let report = normalize(&mut state);
    Ok((state, report))
}

/// Caps an accepted file: newest 500 ids per topic and the 512 newest topic
/// rows. Version 1 has no per-row timestamp beyond the cursor, so recency is
/// `last_time` and ties break on the topic name.
fn normalize(state: &mut State) -> LoadReport {
    let mut report = LoadReport::default();
    for topic in state.topics.values_mut() {
        if topic.last_ids.len() > MAX_PROCESSED_IDS {
            let drop = topic.last_ids.len() - MAX_PROCESSED_IDS;
            topic.last_ids.drain(..drop);
            report.truncated_ids += drop;
        }
    }
    if state.topics.len() > MAX_TOPIC_ROWS {
        let mut keys: Vec<(i64, String)> = state
            .topics
            .iter()
            .map(|(topic, entry)| (entry.last_time, topic.clone()))
            .collect();
        keys.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        let keep: BTreeSet<String> = keys
            .into_iter()
            .take(MAX_TOPIC_ROWS)
            .map(|(_, topic)| topic)
            .collect();
        let before = state.topics.len();
        state.topics.retain(|topic, _| keep.contains(topic));
        report.dropped_topics = before - state.topics.len();
    }
    // Hints follow the topic cap so a long-lived install cannot grow the file
    // without bound. Rows with neither a last nor a pending target are dead.
    state
        .targets
        .retain(|_, target| target.last.is_some() || target.pending.is_some());
    if state.targets.len() > MAX_TOPIC_ROWS {
        let mut keys: Vec<(i64, String)> = state
            .targets
            .iter()
            .map(|(topic, target)| (target.last_at.max(target.pending_at), topic.clone()))
            .collect();
        keys.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        let keep: BTreeSet<String> = keys
            .into_iter()
            .take(MAX_TOPIC_ROWS)
            .map(|(_, topic)| topic)
            .collect();
        state.targets.retain(|topic, _| keep.contains(topic));
    }
    report
}

pub fn write_state_atomic(path: &Path, state: &State) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut temp = OsString::from(path.as_os_str());
    temp.push(format!(".{}.tmp", std::process::id()));
    let temp = PathBuf::from(temp);
    let body = format!(
        "{}\n",
        serde_json::to_string(state)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
    );
    let mut handle = crate::fsutil::owner_only(&temp)?;
    handle.write_all(body.as_bytes())?;
    handle.sync_all()?;
    drop(handle);
    fs::rename(&temp, path)?;
    crate::fsutil::sync_parent(path);
    Ok(())
}

pub fn corrupt_state_path(path: &Path, now_ms: i64) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(format!(".corrupt-{}", now_ms.div_euclid(1000)));
    PathBuf::from(name)
}

pub fn quarantine_state(path: &Path, now_ms: i64) -> io::Result<PathBuf> {
    let target = corrupt_state_path(path, now_ms);
    fs::rename(path, &target)?;
    Ok(target)
}

/// Removes `<state>.corrupt-<unix>` files older than `ttl_secs` and returns the
/// count. Anything that does not parse as a timestamp is left alone, and a
/// missing parent directory is not an error.
pub fn prune_corrupt_backups(path: &Path, now_ms: i64, ttl_secs: i64) -> io::Result<usize> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => return Ok(0),
    };
    let prefix = match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => format!("{name}.corrupt-"),
        None => return Ok(0),
    };
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let now_secs = now_ms.div_euclid(1000);
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stamp) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Ok(seconds) = stamp.parse::<i64>() else {
            continue;
        };
        if now_secs.saturating_sub(seconds) <= ttl_secs {
            continue;
        }
        if !entry.file_type()?.is_file() {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keeps_the_newest_ids_and_the_highest_time() {
        let mut state = State::new();
        state.record_processed("oc-x", "m1", 100);
        state.record_processed("oc-x", "m2", 90);
        let topic = &state.topics["oc-x"];
        assert_eq!(topic.last_time, 100);
        assert_eq!(topic.last_ids, vec!["m1", "m2"]);
    }

    #[test]
    fn window_is_bounded() {
        let mut state = State::new();
        for index in 0..600 {
            state.record_processed("oc-x", &format!("m{index}"), 100 + index);
        }
        let topic = &state.topics["oc-x"];
        assert_eq!(topic.last_ids.len(), MAX_PROCESSED_IDS);
        assert_eq!(topic.last_ids[0], "m100");
    }

    #[test]
    fn targets_are_per_topic_and_clearable() {
        let mut state = State::new();
        state.set_last("oc-x", "ses_1", 100);
        assert_eq!(state.target("oc-x").unwrap().last.as_deref(), Some("ses_1"));
        state.set_pending("oc-x", Some("ses_2"), 110);
        assert_eq!(
            state.target("oc-x").unwrap().pending.as_deref(),
            Some("ses_2")
        );
        state.set_pending("oc-x", None, 120);
        assert_eq!(state.target("oc-x").unwrap().pending, None);
        assert_eq!(state.target("oc-x").unwrap().last.as_deref(), Some("ses_1"));
    }

    #[test]
    fn json_shape_is_compact_and_old_pins_are_ignored() {
        let mut state = State::new();
        state.set_last("oc-x", "ses_1", 100);
        state.record_processed("oc-x", "m1", 100);
        let text = serde_json::to_string(&state).unwrap();
        assert_eq!(
            text,
            r#"{"version":1,"targets":{"oc-x":{"last":"ses_1","lastAt":100,"pendingAt":0}},"topics":{"oc-x":{"lastTime":100,"lastIds":["m1"]}}}"#
        );

        // A 0.10.0 file still loads; the pin is unknown and does not become a target.
        let old = r#"{"version":1,"pins":{"oc-x":"ses_old"},"topics":{}}"#;
        let loaded: State = serde_json::from_str(old).unwrap();
        assert!(loaded.targets.is_empty());
    }
}
