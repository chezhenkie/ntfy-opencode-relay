//! Per-session turn text and file collection.
//!
//! Mirrors `createTurnTracker` in `phone-outbound.js`: parts are buffered from
//! `session.text.ended`, sorted by time then ordinal, joined with two newlines,
//! and published once at the terminal. A terminal is debounced because a
//! completed run emits both `session.execution.succeeded` and a status idle.
//! Timers are deadlines on the state, not one task per turn, so one tick loop
//! drives every session.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ntfy_relay_core::link_safe::link_safe;
use ntfy_relay_core::text::trim_js;

use crate::format::first_characters;
use crate::log::Log;

pub const TURN_DEBOUNCE_MS: i64 = 3_000;
pub const TURN_BUFFER_TTL_MS: i64 = 600_000;
pub const MAX_PENDING_FILES: usize = 64;
pub const MAX_TURN_TEXT_BYTES: usize = 2_097_152;
/// Upper bound on turn states, expired tombstones included. Matches the seen
/// window order of magnitude; a run that touches hundreds of sessions stays
/// bounded and the oldest buffered text is published, not dropped silently.
pub const MAX_TURN_STATES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalMode {
    Idle,
    Interrupted,
    Partial,
}

#[derive(Debug, Clone)]
pub struct TurnSettings {
    pub inline_max_bytes: usize,
    pub attachment_prefix: String,
    pub debounce_ms: i64,
    pub ttl_ms: i64,
}

impl Default for TurnSettings {
    fn default() -> Self {
        Self {
            inline_max_bytes: 3400,
            attachment_prefix: "turn".to_string(),
            debounce_ms: TURN_DEBOUNCE_MS,
            ttl_ms: TURN_BUFFER_TTL_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnBody {
    Text(String),
    Attachment {
        summary: String,
        body: String,
        filename: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOut {
    pub session_id: String,
    /// `None` when the turn had no text; `idle` decides whether a notice goes
    /// out for that empty turn.
    pub body: Option<TurnBody>,
    pub idle: bool,
    /// The body is an incomplete answer (TTL, eviction, suppression or a new
    /// start), not a terminal answer.
    pub partial: bool,
    pub files: Vec<String>,
    pub started_at: Option<i64>,
    /// Assistant message ids carried by `body`; empty when nothing is sent.
    pub messages: Vec<String>,
}

#[derive(Debug)]
struct Part {
    message: String,
    time: i64,
    ordinal: i64,
    text: String,
}

#[derive(Debug)]
struct State {
    parts: Vec<Part>,
    keys: HashSet<String>,
    files: Vec<String>,
    started_at: Option<i64>,
    debounce_at: Option<i64>,
    ttl_at: Option<i64>,
    published: bool,
    open: bool,
    mode: TerminalMode,
    text_bytes: usize,
    truncated: bool,
    /// Insertion order; the lowest sequence is the oldest state.
    seq: u64,
}

impl State {
    fn fresh() -> Self {
        Self {
            parts: Vec::new(),
            keys: HashSet::new(),
            files: Vec::new(),
            started_at: None,
            debounce_at: None,
            ttl_at: None,
            published: false,
            open: false,
            mode: TerminalMode::Idle,
            text_bytes: 0,
            truncated: false,
            seq: 0,
        }
    }
}

pub struct TurnTracker {
    settings: TurnSettings,
    states: HashMap<String, State>,
    active: Option<String>,
    next_seq: u64,
    log: Arc<dyn Log>,
}

impl TurnTracker {
    pub fn new(settings: TurnSettings, log: Arc<dyn Log>) -> Self {
        Self {
            settings,
            states: HashMap::new(),
            active: None,
            next_seq: 0,
            log,
        }
    }

    pub fn pending(&self) -> usize {
        self.states.len()
    }

    fn sequence(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    /// Starts a turn. A pending terminal for the same session is flushed first,
    /// exactly like the plugin's `started`; buffered text with no terminal is
    /// flushed as a partial instead of being discarded.
    pub fn started(&mut self, session_id: &str, now: i64) -> Vec<TurnOut> {
        let mut out = Vec::new();
        if let Some(previous) = self.states.remove(session_id) {
            if previous.debounce_at.is_some() {
                out.extend(self.flush(session_id, previous, now));
            } else if !previous.published && !previous.parts.is_empty() {
                let mut previous = previous;
                previous.mode = TerminalMode::Partial;
                out.extend(self.flush(session_id, previous, now));
            } else if self.active.as_deref() == Some(session_id) {
                self.active = None;
            }
        }
        out.extend(self.reserve(now));
        let mut state = State::fresh();
        state.started_at = Some(now);
        state.open = true;
        state.ttl_at = Some(now + self.settings.ttl_ms);
        state.seq = self.sequence();
        self.active = Some(session_id.to_string());
        self.states.insert(session_id.to_string(), state);
        out
    }

    /// Evicts until one new key fits. Published and expired states go first;
    /// an unfinished state with text is flushed as a partial so the text still
    /// reaches the phone, and every drop leaves one log line.
    fn reserve(&mut self, now: i64) -> Vec<TurnOut> {
        let mut out = Vec::new();
        while self.states.len() >= MAX_TURN_STATES {
            let victim = self
                .states
                .iter()
                .filter(|(_, state)| state.published || state.parts.is_empty())
                .min_by_key(|(_, state)| state.seq)
                .map(|(session_id, _)| session_id.clone())
                .or_else(|| {
                    self.states
                        .iter()
                        .min_by_key(|(_, state)| state.seq)
                        .map(|(session_id, _)| session_id.clone())
                });
            let Some(session_id) = victim else {
                break;
            };
            let Some(state) = self.states.remove(&session_id) else {
                break;
            };
            if self.active.as_deref() == Some(session_id.as_str()) {
                self.active = None;
            }
            if !state.published && !state.parts.is_empty() {
                let mut state = state;
                state.mode = TerminalMode::Partial;
                if let Some(turn) = self.flush_without_keep(&session_id, state, now) {
                    out.push(turn);
                    continue;
                }
            }
            self.log.debug(&format!(
                "turn state cap {MAX_TURN_STATES}: dropped state for session \"{session_id}\""
            ));
        }
        out
    }

    pub fn append(
        &mut self,
        session_id: &str,
        assistant_message_id: &str,
        ordinal: i64,
        text: &str,
        time: i64,
        now: i64,
    ) -> Vec<TurnOut> {
        if text.is_empty() {
            return Vec::new();
        }
        let replace = match self.states.get(session_id) {
            None => true,
            Some(state) => state.published,
        };
        let mut out = Vec::new();
        if replace {
            let existed = self.states.remove(session_id).is_some();
            if !existed {
                out.extend(self.reserve(now));
            }
            let mut state = State::fresh();
            state.seq = self.sequence();
            self.states.insert(session_id.to_string(), state);
        }
        let key = format!("{assistant_message_id}:{ordinal}");
        let state = self.states.get_mut(session_id).expect("state exists");
        if !state.keys.insert(key) {
            return out;
        }
        let part_bytes = text.len();
        if state.truncated || state.text_bytes + part_bytes > MAX_TURN_TEXT_BYTES {
            if !state.truncated {
                state.truncated = true;
                self.log.warn(&format!(
                    "turn text exceeded {MAX_TURN_TEXT_BYTES} bytes; later parts are dropped"
                ));
            }
            return out;
        }
        state.text_bytes += part_bytes;
        state.parts.push(Part {
            message: assistant_message_id.to_string(),
            time,
            ordinal,
            text: text.to_string(),
        });
        state.ttl_at = Some(now + self.settings.ttl_ms);
        out
    }

    /// Ids of turns that started and have not published or expired.
    pub fn open_sessions(&self) -> Vec<String> {
        self.states
            .iter()
            .filter(|(_, state)| state.open)
            .map(|(session_id, _)| session_id.clone())
            .collect()
    }

    /// Removes buffered parts of one assistant message after backfill delivered
    /// it. When nothing is left, the turn is consumed so a late terminal cannot
    /// publish an idle notice for text that already went out.
    pub fn discard_message(&mut self, session_id: &str, message_id: &str) {
        let prefix = format!("{message_id}:");
        let mut consume = false;
        if let Some(state) = self.states.get_mut(session_id) {
            let before = state.parts.len();
            state.parts.retain(|part| part.message != message_id);
            if state.parts.len() != before {
                state.keys.retain(|key| !key.starts_with(&prefix));
                state.text_bytes = state.parts.iter().map(|part| part.text.len()).sum();
            }
            if state.parts.is_empty() && state.open {
                state.published = true;
                state.open = false;
                state.debounce_at = None;
                consume = true;
            }
        }
        if consume && self.active.as_deref() == Some(session_id) {
            self.active = None;
        }
    }

    /// Records a file added while the last started turn is open.
    pub fn add_file(&mut self, path: &str) {
        if path.is_empty() {
            return;
        }
        let Some(active) = self.active.clone() else {
            return;
        };
        let Some(state) = self.states.get_mut(&active) else {
            return;
        };
        if !state.open || state.published {
            return;
        }
        if state.files.len() >= MAX_PENDING_FILES {
            self.log
                .warn("turn file list is full; extra files are not attached");
            return;
        }
        state.files.push(path.to_string());
    }

    pub fn terminal(&mut self, session_id: &str, mode: TerminalMode, now: i64) -> Vec<TurnOut> {
        let mut out = Vec::new();
        if !self.states.contains_key(session_id) {
            out.extend(self.reserve(now));
        }
        let seq = self.sequence();
        let state = self
            .states
            .entry(session_id.to_string())
            .or_insert_with(|| {
                let mut state = State::fresh();
                state.seq = seq;
                state
            });
        if state.published || state.debounce_at.is_some() {
            return out;
        }
        state.mode = mode;
        state.open = false;
        state.debounce_at = Some(now + self.settings.debounce_ms);
        if self.active.as_deref() == Some(session_id) {
            self.active = None;
        }
        out
    }

    /// Publishes buffered text immediately. A failed run with no buffer still
    /// consumes the turn so a later idle frame cannot publish an idle notice.
    pub fn flush_partial(&mut self, session_id: &str, now: i64) -> Vec<TurnOut> {
        let mut out = Vec::new();
        let state = match self.states.remove(session_id) {
            Some(state) => state,
            None => {
                out.extend(self.reserve(now));
                let mut consumed = State::fresh();
                consumed.published = true;
                consumed.ttl_at = Some(now + self.settings.ttl_ms);
                consumed.seq = self.sequence();
                self.states.insert(session_id.to_string(), consumed);
                return out;
            }
        };
        if state.published {
            return out;
        }
        let mut state = state;
        state.mode = TerminalMode::Partial;
        out.extend(self.flush(session_id, state, now));
        out
    }

    pub fn cancel(&mut self, session_id: &str) {
        self.states.remove(session_id);
        if self.active.as_deref() == Some(session_id) {
            self.active = None;
        }
    }

    /// Publishes every due terminal and drops expired buffers. Call on a short
    /// interval; each state's deadline decides when work runs.
    pub fn tick(&mut self, now: i64) -> Vec<TurnOut> {
        let mut due = Vec::new();
        let mut expired = Vec::new();
        for (session_id, state) in &self.states {
            if state.debounce_at.is_some_and(|at| at <= now) {
                due.push(session_id.clone());
            } else if state.ttl_at.is_some_and(|at| at <= now) {
                expired.push(session_id.clone());
            }
        }
        due.sort();
        expired.sort();
        let mut out = Vec::new();
        for session_id in due {
            if let Some(state) = self.states.remove(&session_id) {
                if self.active.as_deref() == Some(session_id.as_str()) {
                    self.active = None;
                }
                out.extend(self.flush(&session_id, state, now));
            }
        }
        for session_id in expired {
            if let Some(state) = self.states.remove(&session_id) {
                // The TTL is the last chance for text whose terminal never
                // arrived; publish it as a partial instead of dropping it.
                self.log.debug(&format!(
                    "turn text buffer expired for session \"{session_id}\"; flushing a partial"
                ));
                let mut state = state;
                state.mode = TerminalMode::Partial;
                out.extend(self.flush(&session_id, state, now));
            }
        }
        out
    }

    /// Drops everything at shutdown; returns how many buffers were discarded.
    pub fn clear(&mut self) -> usize {
        let count = self.states.len();
        self.states.clear();
        self.active = None;
        count
    }

    fn flush(&mut self, session_id: &str, state: State, now: i64) -> Option<TurnOut> {
        self.flush_inner(session_id, state, now, true)
    }

    /// One flush path. `keep` stores the consumed tombstone until the TTL so a
    /// late duplicate terminal cannot publish twice; an evicted state is not
    /// stored again, because the cap needs the slot.
    fn flush_inner(
        &mut self,
        session_id: &str,
        mut state: State,
        now: i64,
        keep: bool,
    ) -> Option<TurnOut> {
        if state.published {
            return None;
        }
        state.published = true;
        state.open = false;
        state.debounce_at = None;
        let mut parts = std::mem::take(&mut state.parts);
        let files = std::mem::take(&mut state.files);
        let started_at = state.started_at;
        state.keys.clear();
        if self.active.as_deref() == Some(session_id) {
            self.active = None;
        }
        let mode = state.mode;
        let mut messages: Vec<String> = parts.iter().map(|part| part.message.clone()).collect();
        messages.sort();
        messages.dedup();
        let trimmed = {
            parts.sort_by(|a, b| a.time.cmp(&b.time).then(a.ordinal.cmp(&b.ordinal)));
            let joined = parts
                .iter()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            trim_js(&joined).to_string()
        };
        let body = compose_body(session_id, &trimmed, mode, &self.settings, now);
        let idle = body.is_none() && mode == TerminalMode::Idle;
        let messages = if body.is_some() { messages } else { Vec::new() };
        // Keep a consumed state until the TTL so a late duplicate terminal
        // cannot publish a second time.
        if keep {
            state.ttl_at = Some(now + self.settings.ttl_ms);
            if state.seq == 0 {
                state.seq = self.sequence();
            }
            self.states.insert(session_id.to_string(), state);
        }
        Some(TurnOut {
            session_id: session_id.to_string(),
            body,
            idle,
            partial: mode == TerminalMode::Partial,
            files,
            started_at,
            messages,
        })
    }

    fn flush_without_keep(&mut self, session_id: &str, state: State, now: i64) -> Option<TurnOut> {
        self.flush_inner(session_id, state, now, false)
    }
}

/// One turn body from already trimmed text. The live flush and the reconnect
/// backfill share this, so both use the same link filter and size split.
pub fn compose_body(
    session_id: &str,
    trimmed: &str,
    mode: TerminalMode,
    settings: &TurnSettings,
    now: i64,
) -> Option<TurnBody> {
    if trimmed.is_empty() {
        return None;
    }
    let raw = if mode == TerminalMode::Interrupted {
        format!("{trimmed}\n\n(interrupted)")
    } else {
        trimmed.to_string()
    };
    let safe = link_safe(&raw);
    if safe.len() <= settings.inline_max_bytes {
        Some(TurnBody::Text(safe))
    } else {
        let summary = format!(
            "{}\n\nFull response attached.",
            first_characters(&safe, 500)
        );
        let short = session_id.chars().take(8).collect::<String>();
        let filename = format!(
            "{}-{}-{}.md",
            settings.attachment_prefix,
            short,
            now.div_euclid(1000)
        );
        Some(TurnBody::Attachment {
            summary,
            body: raw,
            filename,
        })
    }
}

/// One body for a completed assistant message read from the message API.
pub fn recovered_body(
    session_id: &str,
    parts: &[String],
    settings: &TurnSettings,
    now: i64,
) -> Option<TurnBody> {
    let joined = parts
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n\n");
    let trimmed = trim_js(&joined);
    compose_body(session_id, trimmed, TerminalMode::Idle, settings, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::NoopLog;

    fn tracker() -> TurnTracker {
        TurnTracker::new(
            TurnSettings {
                debounce_ms: 100,
                ttl_ms: 10_000,
                ..TurnSettings::default()
            },
            Arc::new(NoopLog),
        )
    }

    fn text(out: &TurnOut) -> &str {
        match &out.body {
            Some(TurnBody::Text(text)) => text,
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn parts_sort_by_time_then_ordinal_and_join_with_blank_lines() {
        let mut tracker = tracker();
        tracker.started("ses_a", 1_000);
        tracker.append("ses_a", "msg", 2, "second", 200, 1_000);
        tracker.append("ses_a", "msg", 1, "first", 200, 1_000);
        tracker.append("ses_a", "msg", 0, "earlier", 100, 1_000);
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        let out = tracker.tick(1_100);
        assert_eq!(out.len(), 1);
        assert_eq!(text(&out[0]), "earlier\n\nfirst\n\nsecond");
        assert!(!out[0].idle);
    }

    #[test]
    fn a_duplicate_part_key_is_ignored() {
        let mut tracker = tracker();
        tracker.started("ses_a", 1_000);
        tracker.append("ses_a", "msg", 1, "one", 100, 1_000);
        tracker.append("ses_a", "msg", 1, "one again", 100, 1_000);
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        let out = tracker.tick(1_100);
        assert_eq!(text(&out[0]), "one");
    }

    #[test]
    fn terminal_is_debounced_until_the_deadline() {
        let mut tracker = tracker();
        tracker.started("ses_a", 1_000);
        tracker.append("ses_a", "msg", 0, "done", 100, 1_000);
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        assert!(tracker.tick(1_099).is_empty());
        let out = tracker.tick(1_100);
        assert_eq!(out.len(), 1);
        assert!(!out[0].partial);
        assert!(tracker.tick(2_000).is_empty());
    }

    #[test]
    fn buffered_text_is_flushed_as_a_partial_at_the_ttl() {
        let mut tracker = tracker();
        tracker.started("ses_a", 1_000);
        tracker.append("ses_a", "msg", 0, "late text", 1, 1_000);
        let out = tracker.tick(1_000 + 10_000);
        assert_eq!(out.len(), 1);
        assert!(out[0].partial);
        assert_eq!(text(&out[0]), "late text");
        assert!(
            tracker.tick(1_000 + 10_100).is_empty(),
            "the flush clears the buffer"
        );
    }

    #[test]
    fn an_empty_idle_turn_still_notifies_once() {
        let mut tracker = tracker();
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        let out = tracker.tick(1_100);
        assert_eq!(out.len(), 1);
        assert!(out[0].idle);
        assert!(out[0].body.is_none());
        assert!(tracker.tick(1_200).is_empty());
    }

    #[test]
    fn interrupted_text_carries_the_marker() {
        let mut tracker = tracker();
        tracker.started("ses_a", 1_000);
        tracker.append("ses_a", "msg", 0, "partial", 100, 1_000);
        tracker.terminal("ses_a", TerminalMode::Interrupted, 1_000);
        let out = tracker.tick(1_100);
        assert_eq!(text(&out[0]), "partial\n\n(interrupted)");
        assert!(!out[0].idle);
    }

    #[test]
    fn an_empty_interrupted_turn_publishes_nothing() {
        let mut tracker = tracker();
        tracker.terminal("ses_a", TerminalMode::Interrupted, 1_000);
        let out = tracker.tick(1_100);
        assert!(out[0].body.is_none());
        assert!(!out[0].idle);
    }

    #[test]
    fn long_text_becomes_a_markdown_attachment() {
        let mut tracker = TurnTracker::new(
            TurnSettings {
                inline_max_bytes: 10,
                debounce_ms: 10,
                ..TurnSettings::default()
            },
            Arc::new(NoopLog),
        );
        tracker.started("ses_abcdefghij", 1_000);
        tracker.append("ses_abcdefghij", "msg", 0, "0123456789abcdef", 1, 1_000);
        tracker.terminal("ses_abcdefghij", TerminalMode::Idle, 1_000);
        let out = tracker.tick(2_000);
        let Some(TurnBody::Attachment {
            summary,
            body,
            filename,
        }) = &out[0].body
        else {
            panic!("expected attachment");
        };
        assert_eq!(body, "0123456789abcdef");
        assert!(summary.ends_with("Full response attached."));
        assert!(filename.starts_with("turn-ses_abcd-2.md"), "{filename}");
    }

    #[test]
    fn turn_body_is_link_safe_but_the_attachment_stays_raw() {
        let mut tracker = TurnTracker::new(
            TurnSettings {
                inline_max_bytes: 10,
                ..TurnSettings::default()
            },
            Arc::new(NoopLog),
        );
        tracker.started("ses_a", 1_000);
        tracker.append("ses_a", "msg", 0, "open readme.md now", 1, 1_000);
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        let out = tracker.tick(5_000);
        let Some(TurnBody::Attachment { summary, body, .. }) = &out[0].body else {
            panic!("expected attachment");
        };
        assert!(summary.starts_with("open readme[.]md now"), "{summary}");
        assert_eq!(body, "open readme.md now");
    }

    #[test]
    fn a_late_duplicate_terminal_cannot_publish_twice() {
        let mut tracker = tracker();
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        assert_eq!(tracker.tick(1_100).len(), 1);
        tracker.terminal("ses_a", TerminalMode::Idle, 1_200);
        assert!(tracker.tick(1_300).is_empty());
        // A new turn may start and publish normally.
        tracker.started("ses_a", 1_400);
        tracker.append("ses_a", "msg", 0, "next", 1, 1_400);
        tracker.terminal("ses_a", TerminalMode::Idle, 1_400);
        assert_eq!(tracker.tick(1_500).len(), 1);
    }

    #[test]
    fn files_attach_only_while_the_turn_is_open() {
        let mut tracker = tracker();
        tracker.add_file("ignored.txt");
        tracker.started("ses_a", 1_000);
        tracker.add_file("one.png");
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        tracker.add_file("late.png");
        let out = tracker.tick(1_100);
        assert_eq!(out[0].files, vec!["one.png".to_string()]);
    }

    #[test]
    fn a_started_turn_flushes_a_pending_terminal() {
        let mut tracker = tracker();
        tracker.started("ses_a", 1_000);
        tracker.append("ses_a", "msg", 0, "first", 1, 1_000);
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        let out = tracker.started("ses_a", 1_200);
        assert_eq!(out.len(), 1);
        assert_eq!(text(&out[0]), "first");
    }

    #[test]
    fn a_new_start_flushes_buffered_text_without_a_terminal() {
        let mut tracker = tracker();
        tracker.started("ses_a", 1_000);
        tracker.append("ses_a", "msg", 0, "buffered", 1, 1_000);
        let out = tracker.started("ses_a", 1_100);
        assert_eq!(out.len(), 1);
        assert!(out[0].partial);
        assert_eq!(text(&out[0]), "buffered");
        assert!(!text(&out[0]).contains("interrupted"));
        assert!(tracker.tick(2_000).is_empty());
    }

    #[test]
    fn the_state_map_stays_bounded_and_flushes_the_oldest_text() {
        let mut tracker = tracker();
        for index in 0..MAX_TURN_STATES {
            let id = format!("ses_{index}");
            tracker.started(&id, 1_000 + index as i64);
            tracker.append(&id, "msg", 0, &format!("text {index}"), 1, 1_000);
        }
        assert_eq!(tracker.pending(), MAX_TURN_STATES);
        let mut out = tracker.started("ses_new", 2_000);
        assert_eq!(tracker.pending(), MAX_TURN_STATES);
        assert_eq!(out.len(), 1, "the oldest buffered text is flushed");
        assert_eq!(text(&out.remove(0)), "text 0");
    }

    #[test]
    fn three_hundred_empty_starts_stay_bounded() {
        let mut tracker = tracker();
        for index in 0..300 {
            tracker.started(&format!("ses_{index}"), 1_000);
        }
        assert_eq!(tracker.pending(), MAX_TURN_STATES);
    }

    #[test]
    fn a_failed_run_without_text_consumes_the_turn() {
        let mut tracker = tracker();
        tracker.started("ses_a", 1_000);
        let out = tracker.flush_partial("ses_a", 1_000);
        assert_eq!(out.len(), 1);
        assert!(out[0].body.is_none());
        assert!(!out[0].idle);
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        assert!(tracker.tick(2_000).is_empty());
    }

    #[test]
    fn the_text_cap_drops_later_parts_once() {
        let mut tracker = TurnTracker::new(TurnSettings::default(), Arc::new(NoopLog));
        tracker.started("ses_a", 1_000);
        let huge = "x".repeat(MAX_TURN_TEXT_BYTES);
        tracker.append("ses_a", "msg", 0, &huge, 1, 1_000);
        tracker.append("ses_a", "msg", 1, "after cap", 2, 1_000);
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        let out = tracker.tick(4_000);
        let Some(TurnBody::Attachment { body, .. }) = &out[0].body else {
            panic!("expected attachment");
        };
        assert!(!body.contains("after cap"));
    }

    #[test]
    fn idle_buffers_expire_after_the_ttl() {
        let mut tracker = tracker();
        tracker.started("ses_a", 1_000);
        assert_eq!(tracker.pending(), 1);
        let out = tracker.tick(1_000 + 10_000);
        assert_eq!(out.len(), 1, "an empty state still leaves one output");
        assert!(out[0].body.is_none());
        assert!(!out[0].idle);
        assert_eq!(tracker.pending(), 1, "the consumed tombstone is kept");
        assert!(tracker.tick(1_000 + 20_000).is_empty());
    }

    #[test]
    fn a_turn_body_names_every_message_it_carries() {
        let mut tracker = tracker();
        tracker.started("ses_a", 1_000);
        tracker.append("ses_a", "msg_1", 0, "one", 100, 1_000);
        tracker.append("ses_a", "msg_1", 1, "two", 200, 1_000);
        tracker.append("ses_a", "msg_2", 0, "three", 300, 1_000);
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        let out = tracker.tick(1_100);
        assert_eq!(
            out[0].messages,
            vec!["msg_1".to_string(), "msg_2".to_string()]
        );
        assert_eq!(text(&out[0]), "one\n\ntwo\n\nthree");
    }

    #[test]
    fn a_recovered_message_uses_the_turn_thresholds() {
        let settings = TurnSettings {
            inline_max_bytes: 10,
            ..TurnSettings::default()
        };
        let short = recovered_body("ses_a", &["one".to_string()], &settings, 5_000).unwrap();
        assert_eq!(short, TurnBody::Text("one".to_string()));
        let long =
            recovered_body("ses_a", &["0123456789abcdef".to_string()], &settings, 5_000).unwrap();
        let TurnBody::Attachment {
            filename, summary, ..
        } = long
        else {
            panic!("expected attachment");
        };
        assert!(filename.starts_with("turn-ses_a-5.md"), "{filename}");
        assert!(summary.ends_with("Full response attached."));
        assert_eq!(
            recovered_body("ses_a", &["  ".to_string()], &settings, 5_000),
            None
        );
    }

    #[test]
    fn a_discarded_message_leaves_the_other_parts_buffered() {
        let mut tracker = tracker();
        tracker.started("ses_a", 1_000);
        tracker.append("ses_a", "msg_1", 0, "one", 1, 1_000);
        tracker.append("ses_a", "msg_2", 0, "two", 2, 1_000);
        tracker.discard_message("ses_a", "msg_1");
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        let out = tracker.tick(1_100);
        assert_eq!(text(&out[0]), "two");
        assert_eq!(out[0].messages, vec!["msg_2".to_string()]);
    }

    #[test]
    fn discarding_the_last_message_consumes_the_turn() {
        let mut tracker = tracker();
        tracker.started("ses_a", 1_000);
        tracker.append("ses_a", "msg_1", 0, "one", 1, 1_000);
        tracker.discard_message("ses_a", "msg_1");
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        assert!(tracker.tick(1_100).is_empty());
        assert!(tracker.open_sessions().is_empty());
    }

    #[test]
    fn open_sessions_lists_turns_until_they_publish() {
        let mut tracker = tracker();
        tracker.started("ses_a", 1_000);
        tracker.append("ses_a", "msg_1", 0, "one", 1, 1_000);
        assert_eq!(tracker.open_sessions(), vec!["ses_a".to_string()]);
        tracker.terminal("ses_a", TerminalMode::Idle, 1_000);
        assert!(tracker.open_sessions().is_empty());
        tracker.tick(1_100);
        assert!(tracker.open_sessions().is_empty());
    }
}
