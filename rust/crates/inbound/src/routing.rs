//! Target selection for a phone message.
//!
//! Routing is derived, never stored. A message resolves against the live
//! session snapshot plus two short-lived hints: the session that accepted the
//! previous prompt, and a one-shot target from `/new` or `/to`. A hint outside
//! its window is ignored, so no decision can capture traffic after the desktop
//! has moved on.
//!
//! Order: the pending target, then the previous target inside the continuity
//! window, then the session with the newest activity, where activity is the
//! later of `viewed` (desktop use) and `updated` (any message). A child session
//! is never a target. Ties break on running, then the previous target, then id,
//! so the same snapshot always resolves to the same session.

use ntfy_relay_core::adapter::{Session, SessionSignal};

/// Session window used for routing, matching `fetchSessions`.
pub const ROUTE_SESSION_LIMIT: usize = 20;

/// The previous target stays valid while the phone keeps using it.
pub const CONTINUITY_MS: i64 = 10 * 60 * 1000;
/// A pending target set by `/new` or `/to` is one message, or this long.
pub const PENDING_MS: i64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// One-shot target from `/new` or `/to`.
    Pending,
    /// The session that accepted the previous prompt.
    Continuity,
    /// Newest activity in the project.
    Activity,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Continuity => "continuity",
            Self::Activity => "activity",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hints<'a> {
    pub last: Option<&'a str>,
    pub last_at: i64,
    pub pending: Option<&'a str>,
    pub pending_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Windows {
    pub continuity_ms: i64,
    pub pending_ms: i64,
}

impl Default for Windows {
    fn default() -> Self {
        Self {
            continuity_ms: CONTINUITY_MS,
            pending_ms: PENDING_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub session: Session,
    pub source: Source,
}

fn fresh(at: i64, now: i64, window_ms: i64) -> bool {
    at > 0 && at <= now && now - at <= window_ms
}

fn find<'a>(signals: &'a [SessionSignal], id: &str) -> Option<&'a SessionSignal> {
    signals
        .iter()
        .find(|signal| !signal.session.id.is_empty() && signal.session.id == id)
}

fn activity(signal: &SessionSignal) -> i64 {
    signal.viewed.max(signal.session.updated)
}

fn choose<'a>(
    signals: &'a [SessionSignal],
    prefer_running: bool,
    last: Option<&str>,
) -> Option<&'a SessionSignal> {
    let last_id = last.unwrap_or_default();
    signals
        .iter()
        .filter(|signal| !signal.session.id.is_empty() && signal.parent_id.is_none())
        .max_by(|left, right| {
            let left_running = prefer_running && left.session.running;
            let right_running = prefer_running && right.session.running;
            activity(left)
                .cmp(&activity(right))
                .then_with(|| left_running.cmp(&right_running))
                .then_with(|| (left.session.id == last_id).cmp(&(right.session.id == last_id)))
                // Smaller id wins a full tie; reverse the comparison so `max_by` picks it.
                .then_with(|| right.session.id.cmp(&left.session.id))
        })
}

/// Resolve one delivery target. Pure: the caller supplies the snapshot, the
/// hints, and the clock.
pub fn resolve(
    signals: &[SessionSignal],
    hints: Hints<'_>,
    now: i64,
    prefer_running: bool,
    windows: Windows,
) -> Option<Resolved> {
    if let Some(id) = hints.pending {
        if fresh(hints.pending_at, now, windows.pending_ms) {
            if let Some(signal) = find(signals, id) {
                return Some(Resolved {
                    session: signal.session.clone(),
                    source: Source::Pending,
                });
            }
        }
    }
    if let Some(id) = hints.last {
        if fresh(hints.last_at, now, windows.continuity_ms) {
            if let Some(signal) = find(signals, id) {
                // The hint only holds the thread while nothing else was active
                // after the previous prompt. A desktop view or a message in
                // another session is newer evidence and wins.
                let newest_other = signals
                    .iter()
                    .filter(|other| other.session.id != id && !other.session.id.is_empty())
                    .filter(|other| other.parent_id.is_none())
                    .map(activity)
                    .max()
                    .unwrap_or(i64::MIN);
                if newest_other <= hints.last_at {
                    return Some(Resolved {
                        session: signal.session.clone(),
                        source: Source::Continuity,
                    });
                }
            }
        }
    }
    choose(signals, prefer_running, hints.last).map(|signal| Resolved {
        session: signal.session.clone(),
        source: Source::Activity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(id: &str, updated: i64, viewed: i64, running: bool) -> SessionSignal {
        SessionSignal {
            session: Session {
                id: id.to_string(),
                title: Some(id.to_string()),
                running,
                updated,
                project_id: Some("p1".to_string()),
            },
            viewed,
            parent_id: None,
        }
    }

    fn target(id: &str) -> Option<Resolved> {
        resolve(
            &[
                signal("ses_old", 100, 100, false),
                signal("ses_current", 200, 200, false),
            ],
            Hints {
                last: Some(id),
                last_at: 250,
                ..Hints::default()
            },
            300,
            true,
            Windows::default(),
        )
    }

    #[test]
    fn a_stale_target_loses_to_current_activity() {
        // The 0.10.0 failure: a two-day-old target must not capture the message.
        let now = 2 * 24 * 60 * 60 * 1000;
        let resolved = resolve(
            &[
                signal("ses_old", 1, 1, false),
                signal("ses_current", now - 1, now - 1, false),
            ],
            Hints {
                last: Some("ses_old"),
                last_at: 1_000,
                ..Hints::default()
            },
            now,
            true,
            Windows::default(),
        )
        .unwrap();
        assert_eq!(resolved.session.id, "ses_current");
        assert_eq!(resolved.source, Source::Activity);
    }

    #[test]
    fn a_recent_target_wins_for_continuity() {
        let resolved = target("ses_old").unwrap();
        assert_eq!(resolved.session.id, "ses_old");
        assert_eq!(resolved.source, Source::Continuity);
    }

    #[test]
    fn a_newer_desktop_view_breaks_continuity() {
        // The phone talked to ses_old at 100. The desktop opened ses_current
        // afterwards, so the hint must not capture the next message.
        let resolved = resolve(
            &[
                signal("ses_old", 100, 100, false),
                signal("ses_current", 100, 150, false),
            ],
            Hints {
                last: Some("ses_old"),
                last_at: 100,
                ..Hints::default()
            },
            300,
            true,
            Windows::default(),
        )
        .unwrap();
        assert_eq!(resolved.session.id, "ses_current");
        assert_eq!(resolved.source, Source::Activity);
    }

    #[test]
    fn a_pending_target_wins_over_continuity() {
        let resolved = resolve(
            &[
                signal("ses_old", 100, 100, false),
                signal("ses_current", 200, 200, false),
            ],
            Hints {
                last: Some("ses_old"),
                last_at: 150,
                pending: Some("ses_current"),
                pending_at: 200,
            },
            300,
            true,
            Windows::default(),
        )
        .unwrap();
        assert_eq!(resolved.session.id, "ses_current");
        assert_eq!(resolved.source, Source::Pending);
    }

    #[test]
    fn an_expired_pending_target_is_ignored() {
        let resolved = resolve(
            &[
                signal("ses_a", 100, 100, false),
                signal("ses_b", 200, 200, false),
            ],
            Hints {
                pending: Some("ses_a"),
                pending_at: 100,
                ..Hints::default()
            },
            700_000,
            true,
            Windows::default(),
        )
        .unwrap();
        assert_eq!(resolved.session.id, "ses_b");
    }

    #[test]
    fn a_child_session_is_never_a_target() {
        let mut child = signal("ses_child", 300, 300, false);
        child.parent_id = Some("ses_parent".to_string());
        let resolved = resolve(
            &[signal("ses_a", 100, 100, false), child],
            Hints::default(),
            400,
            true,
            Windows::default(),
        )
        .unwrap();
        assert_eq!(resolved.session.id, "ses_a");
    }

    #[test]
    fn viewed_is_activity_so_the_open_chat_wins() {
        let resolved = resolve(
            &[
                signal("ses_typed", 200, 0, false),
                signal("ses_open", 100, 250, false),
            ],
            Hints::default(),
            300,
            true,
            Windows::default(),
        )
        .unwrap();
        assert_eq!(resolved.session.id, "ses_open");
    }

    #[test]
    fn running_breaks_an_activity_tie() {
        let resolved = resolve(
            &[
                signal("ses_idle", 200, 200, false),
                signal("ses_running", 200, 200, true),
            ],
            Hints::default(),
            300,
            true,
            Windows::default(),
        )
        .unwrap();
        assert_eq!(resolved.session.id, "ses_running");
    }

    #[test]
    fn the_previous_target_breaks_an_activity_tie() {
        let resolved = resolve(
            &[
                signal("ses_a", 200, 200, false),
                signal("ses_b", 200, 200, false),
            ],
            Hints {
                last: Some("ses_b"),
                last_at: 100,
                ..Hints::default()
            },
            700_000,
            true,
            Windows::default(),
        )
        .unwrap();
        assert_eq!(resolved.session.id, "ses_b");
        assert_eq!(resolved.source, Source::Activity);
    }

    #[test]
    fn a_full_tie_is_deterministic() {
        let first = resolve(
            &[
                signal("ses_b", 200, 200, false),
                signal("ses_a", 200, 200, false),
            ],
            Hints::default(),
            300,
            true,
            Windows::default(),
        )
        .unwrap();
        let second = resolve(
            &[
                signal("ses_a", 200, 200, false),
                signal("ses_b", 200, 200, false),
            ],
            Hints::default(),
            300,
            true,
            Windows::default(),
        )
        .unwrap();
        assert_eq!(first.session.id, "ses_a");
        assert_eq!(second.session.id, "ses_a");
    }

    #[test]
    fn prefer_running_off_ignores_the_running_flag() {
        let resolved = resolve(
            &[
                signal("ses_idle", 300, 300, false),
                signal("ses_running", 200, 200, true),
            ],
            Hints::default(),
            400,
            false,
            Windows::default(),
        )
        .unwrap();
        assert_eq!(resolved.session.id, "ses_idle");
    }

    #[test]
    fn an_empty_snapshot_has_no_target() {
        assert!(resolve(&[], Hints::default(), 1, true, Windows::default()).is_none());
    }

    #[test]
    fn a_pending_id_missing_from_the_snapshot_falls_back() {
        let resolved = resolve(
            &[signal("ses_a", 100, 100, false)],
            Hints {
                pending: Some("ses_gone"),
                pending_at: 100,
                ..Hints::default()
            },
            200,
            true,
            Windows::default(),
        )
        .unwrap();
        assert_eq!(resolved.session.id, "ses_a");
        assert_eq!(resolved.source, Source::Activity);
    }
}
