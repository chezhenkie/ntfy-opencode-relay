//! Delayed permission pushes.
//!
//! The plugin waits `permissionNotificationDelayMs` so a fast manual answer
//! lands before the phone hears about the ask. A `permission.replied` cancels
//! the pending push, including the race where the reply is observed before its
//! ask. Deadline scheduling keeps one timer per request without a task per
//! request.
//!
//! `tick` is non-destructive: the worker registers and publishes a due notice,
//! then calls `complete`. A persist failure leaves the entry pending so the
//! next tick retries the same deterministic code.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::log::Log;

/// Pending permission timers held at once, matching `MAX_PENDING_PERMISSION_TIMERS`.
pub const MAX_PENDING: usize = 512;
/// Recently answered request IDs kept to suppress a late ask.
pub const MAX_RECENTLY_REPLIED: usize = 512;

#[derive(Debug, Clone)]
pub struct PendingNotice {
    pub session_id: String,
    /// Session title at push time, used in the ack and the registry.
    pub title: String,
    pub topic: String,
    pub permission: String,
    pub patterns: Vec<String>,
}

pub struct PermissionScheduler {
    delay_ms: i64,
    pending: HashMap<String, (i64, PendingNotice)>,
    /// Request ids already pushed or answered; suppresses a duplicate ask and a
    /// reply observed before its ask, at any delay including zero.
    known: VecDeque<String>,
    overflow_warned: bool,
    log: Arc<dyn Log>,
}

impl PermissionScheduler {
    pub fn new(delay_ms: i64, log: Arc<dyn Log>) -> Self {
        Self {
            delay_ms,
            pending: HashMap::new(),
            known: VecDeque::new(),
            overflow_warned: false,
            log,
        }
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Returns a notice when the delay is zero; otherwise schedules one. The
    /// replied and duplicate checks run first so a zero delay keeps the same
    /// suppression as a delayed push.
    pub fn asked(
        &mut self,
        request_id: &str,
        notice: PendingNotice,
        now: i64,
    ) -> Option<PendingNotice> {
        if self.known.contains(&request_id.to_string()) {
            return None;
        }
        if self.delay_ms <= 0 {
            return Some(notice);
        }
        if let Some(pending) = self.pending.get_mut(request_id) {
            // A replay refreshes the notice text but keeps the original
            // deadline, so it cannot postpone its own push.
            pending.1 = notice;
            return None;
        }
        if self.pending_count() >= MAX_PENDING {
            if !self.overflow_warned {
                self.overflow_warned = true;
                self.log.warn(
                    "permission notification skipped: too many pending permission notifications",
                );
            }
            return None;
        }
        self.pending
            .insert(request_id.to_string(), (now + self.delay_ms, notice));
        None
    }

    /// Cancels a pending push and remembers the ID.
    pub fn replied(&mut self, request_id: &str) {
        if self.pending.remove(request_id).is_some() && self.pending_count() < MAX_PENDING {
            self.overflow_warned = false;
        }
        self.remember(request_id);
    }

    /// Marks a due entry handled. The worker calls it after the registry write
    /// and the push, whatever their outcome.
    pub fn complete(&mut self, request_id: &str) {
        if self.pending.remove(request_id).is_some() && self.pending_count() < MAX_PENDING {
            self.overflow_warned = false;
        }
        self.remember(request_id);
    }

    fn remember(&mut self, request_id: &str) {
        if let Some(index) = self.known.iter().position(|id| id == request_id) {
            self.known.remove(index);
        }
        self.known.push_back(request_id.to_string());
        while self.known.len() > MAX_RECENTLY_REPLIED {
            self.known.pop_front();
        }
    }

    /// Due entries in deadline order, oldest first, without removing them.
    pub fn tick(&mut self, now: i64) -> Vec<(String, PendingNotice)> {
        let mut due: Vec<(String, i64, PendingNotice)> = self
            .pending
            .iter()
            .filter(|(_, (at, _))| *at <= now)
            .map(|(id, (at, notice))| (id.clone(), *at, notice.clone()))
            .collect();
        due.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
        due.into_iter()
            .map(|(id, _, notice)| (id, notice))
            .collect()
    }

    pub fn clear(&mut self) -> usize {
        let count = self.pending.len();
        self.pending.clear();
        self.known.clear();
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::NoopLog;

    fn notice() -> PendingNotice {
        PendingNotice {
            session_id: "ses_1".to_string(),
            title: "Fix links".to_string(),
            topic: "proj".to_string(),
            permission: "bash".to_string(),
            patterns: vec!["git status".to_string()],
        }
    }

    fn scheduler(delay: i64) -> PermissionScheduler {
        PermissionScheduler::new(delay, Arc::new(NoopLog))
    }

    #[test]
    fn a_zero_delay_returns_the_notice_immediately() {
        let mut scheduler = scheduler(0);
        let due = scheduler.asked("per_1", notice(), 1_000).unwrap();
        assert_eq!(due.session_id, "ses_1");
        assert!(scheduler.tick(2_000).is_empty());
    }

    #[test]
    fn a_delayed_push_waits_for_its_deadline_and_tick_keeps_the_entry() {
        let mut scheduler = scheduler(15_000);
        assert!(scheduler.asked("per_1", notice(), 1_000).is_none());
        assert!(scheduler.tick(15_999).is_empty());
        let due = scheduler.tick(16_000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, "per_1");
        assert_eq!(due[0].1.permission, "bash");
        assert_eq!(scheduler.pending_count(), 1, "tick does not consume");
        scheduler.complete("per_1");
        assert_eq!(scheduler.pending_count(), 0);
        assert!(scheduler.tick(17_000).is_empty());
    }

    #[test]
    fn a_reply_cancels_the_pending_push() {
        let mut scheduler = scheduler(15_000);
        scheduler.asked("per_1", notice(), 1_000);
        scheduler.replied("per_1");
        assert!(scheduler.tick(20_000).is_empty());
    }

    #[test]
    fn a_reply_seen_before_the_ask_suppresses_it() {
        let mut scheduler = scheduler(15_000);
        scheduler.replied("per_1");
        assert!(scheduler.asked("per_1", notice(), 1_000).is_none());
        assert_eq!(scheduler.pending_count(), 0);
    }

    #[test]
    fn a_duplicate_ask_is_ignored_while_pending() {
        let mut scheduler = scheduler(15_000);
        scheduler.asked("per_1", notice(), 1_000);
        scheduler.asked("per_1", notice(), 1_500);
        assert_eq!(scheduler.pending_count(), 1);
    }

    #[test]
    fn a_replayed_ask_keeps_its_deadline_and_takes_the_new_text() {
        let mut scheduler = scheduler(15_000);
        scheduler.asked("per_1", notice(), 1_000);
        let mut changed = notice();
        changed.permission = "edit".to_string();
        scheduler.asked("per_1", changed, 5_000);
        assert!(scheduler.tick(15_999).is_empty());
        let due = scheduler.tick(16_000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].1.permission, "edit");
    }

    #[test]
    fn a_completed_ask_suppresses_a_duplicate() {
        let mut scheduler = scheduler(0);
        assert!(scheduler.asked("per_1", notice(), 1_000).is_some());
        scheduler.complete("per_1");
        assert!(scheduler.asked("per_1", notice(), 1_000).is_none());
    }

    #[test]
    fn a_zero_delay_ask_after_a_reply_is_suppressed() {
        let mut scheduler = scheduler(0);
        scheduler.replied("per_1");
        assert!(scheduler.asked("per_1", notice(), 1_000).is_none());
    }

    #[test]
    fn a_sent_delayed_push_suppresses_a_later_duplicate() {
        let mut scheduler = scheduler(10);
        scheduler.asked("per_1", notice(), 1_000);
        let due = scheduler.tick(2_000);
        assert_eq!(due.len(), 1);
        scheduler.complete("per_1");
        assert!(scheduler.asked("per_1", notice(), 3_000).is_none());
    }

    #[test]
    fn due_entries_come_oldest_first_and_the_cap_is_enforced() {
        let mut scheduler = scheduler(1_000);
        for index in 0..MAX_PENDING {
            scheduler.asked(&format!("per_{index}"), notice(), 1_000);
        }
        scheduler.asked("per_overflow", notice(), 1_000);
        assert_eq!(scheduler.pending_count(), MAX_PENDING);
        let due = scheduler.tick(2_000);
        assert_eq!(due.len(), MAX_PENDING);
        assert_eq!(due[0].0, "per_0");
    }

    #[test]
    fn the_recently_replied_list_is_bounded() {
        let mut scheduler = scheduler(15_000);
        for index in 0..(MAX_RECENTLY_REPLIED + 10) {
            scheduler.replied(&format!("per_{index}"));
        }
        assert_eq!(scheduler.known.len(), MAX_RECENTLY_REPLIED);
        assert!(!scheduler.known.contains(&"per_0".to_string()));
    }
}
