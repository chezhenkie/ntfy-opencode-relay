//! Background lane for slow outbound work.
//!
//! The event worker must answer a control event within a tick, so the backfill
//! API queries and the per-turn file scan and upload run in spawned tasks. Each
//! task reports through one channel; the worker keeps the journal, the turn
//! tracker and the feed cursor, so dedupe and ordering stay in one place.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use ntfy_relay_core::topic::{ProjectRoots, TopicOwners};
use ntfy_relay_opencode::api::{Api, ApiError};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::log::Log;
use crate::runner::clip;
use crate::scope::within_roots;
use crate::topic::SessionResolver;
use crate::turn::{self, TurnBody, TurnSettings};

pub const BACKFILL_PAGE: usize = 20;
pub const BACKFILL_MAX_PAGES: usize = 10;
/// Messages fetched by one reconnect or start.
pub const BACKFILL_MAX_MESSAGES: usize = 200;
/// Sessions read per project root on a cold start.
pub const BACKFILL_COLD_SESSIONS: usize = 20;
pub const BACKFILL_TIMEOUT: Duration = Duration::from_secs(5);

/// Report from a spawned lane task to the event worker.
pub enum LaneMsg {
    /// One completed message recovered by a backfill fetch.
    Backfill(BackfillItem),
    /// The fetch pass ended.
    Done(Done),
    /// The auto-attach task published its files.
    Attached {
        topic: String,
        published: usize,
        skipped: usize,
        bytes: u64,
    },
}

pub struct BackfillItem {
    pub session_id: String,
    pub topic: String,
    pub message_id: String,
    pub body: TurnBody,
}

pub struct Done {
    pub generation: u64,
    pub cold: bool,
    pub gap_ms: i64,
    pub sessions: usize,
    pub fetched: usize,
}

pub struct BackfillRequest {
    pub generation: u64,
    pub cold: bool,
    pub from: i64,
    pub gap_ms: i64,
    pub now: i64,
    pub focus: Vec<String>,
    pub open: Vec<String>,
    pub last_text: Option<String>,
}

/// Fetches one gap pass and streams every completed message to the worker.
/// The worker publishes and journals it, so a live part and a recovered copy
/// still dedupe through one seen window.
pub async fn backfill(
    request: BackfillRequest,
    api: Arc<dyn Api>,
    log: Arc<dyn Log>,
    roots: Arc<RwLock<ProjectRoots>>,
    owners: Arc<RwLock<TopicOwners>>,
    settings: TurnSettings,
    out: mpsc::Sender<LaneMsg>,
) {
    let mut resolver = SessionResolver::new(roots, owners);
    let scope = scope(&request, api.as_ref(), &resolver.roots(), log.as_ref()).await;
    let sessions = scope.len();
    let mut fetched = 0usize;
    let mut budget = BACKFILL_MAX_MESSAGES;
    for session_id in scope {
        if budget == 0 {
            break;
        }
        match fetch_session(
            &mut resolver,
            api.as_ref(),
            &settings,
            &session_id,
            request.from,
            request.now,
            &mut budget,
            &out,
        )
        .await
        {
            Ok(messages) => fetched += messages,
            Err(error) => log.warn(&format!(
                "backfill session={session_id} failed: {}; live stream stays up",
                clip(error.message(), 200)
            )),
        }
    }
    let _ = out
        .send(LaneMsg::Done(Done {
            generation: request.generation,
            cold: request.cold,
            gap_ms: request.gap_ms,
            sessions,
            fetched,
        }))
        .await;
}

async fn scope(
    request: &BackfillRequest,
    api: &dyn Api,
    roots: &[std::path::PathBuf],
    log: &dyn Log,
) -> Vec<String> {
    let mut scope: Vec<String> = Vec::new();
    for id in &request.focus {
        push_scope(&mut scope, id);
    }
    if !request.cold {
        for id in &request.open {
            push_scope(&mut scope, id);
        }
        if let Some(last) = request.last_text.as_deref() {
            push_scope(&mut scope, last);
        }
        return scope;
    }
    let projects = match timeout(BACKFILL_TIMEOUT, api.projects()).await {
        Ok(Ok(projects)) => projects,
        Ok(Err(error)) => {
            log.warn(&format!(
                "backfill project query failed: {}; live stream stays up",
                clip(error.message(), 200)
            ));
            return scope;
        }
        Err(_) => {
            log.warn("backfill project query timed out");
            return scope;
        }
    };
    let mut candidates: Vec<(usize, i64, String)> = Vec::new();
    for project in projects {
        let Some(canonical) = project.canonical.as_deref() else {
            continue;
        };
        let Some(root) = roots.iter().position(|root| {
            within_roots(std::path::Path::new(canonical), std::slice::from_ref(root))
        }) else {
            continue;
        };
        match timeout(
            BACKFILL_TIMEOUT,
            api.sessions(&project.id, BACKFILL_COLD_SESSIONS),
        )
        .await
        {
            Ok(Ok(sessions)) => {
                for session in sessions {
                    if !candidates.iter().any(|(_, _, id)| id == &session.id) {
                        candidates.push((root, session.time.updated, session.id));
                    }
                }
            }
            Ok(Err(error)) => log.warn(&format!(
                "backfill session list {} failed: {}",
                project.id,
                clip(error.message(), 200)
            )),
            Err(_) => log.warn(&format!("backfill session list {} timed out", project.id)),
        }
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.1));
    let mut per_root = vec![0usize; roots.len()];
    for (root, _, id) in candidates {
        if per_root[root] < BACKFILL_COLD_SESSIONS {
            per_root[root] += 1;
            push_scope(&mut scope, &id);
        }
    }
    scope
}

#[allow(clippy::too_many_arguments)]
async fn fetch_session(
    resolver: &mut SessionResolver,
    api: &dyn Api,
    settings: &TurnSettings,
    session_id: &str,
    from: i64,
    now: i64,
    budget: &mut usize,
    out: &mpsc::Sender<LaneMsg>,
) -> Result<usize, ApiError> {
    let meta = resolver.meta_with(api, session_id, None, now, true).await;
    let Some(topic) = meta.topic.clone() else {
        return Ok(0);
    };
    if meta.is_child() {
        return Ok(0);
    }
    let mut cursor: Option<String> = None;
    let mut collected = Vec::new();
    let mut fetched = 0usize;
    for _ in 0..BACKFILL_MAX_PAGES {
        if *budget == 0 {
            break;
        }
        let page = timeout(
            BACKFILL_TIMEOUT,
            api.messages(session_id, BACKFILL_PAGE, cursor.as_deref()),
        )
        .await
        .map_err(|_| ApiError::new("message query timed out"))??;
        let mut page_has_old = false;
        for message in page.messages {
            let Some(completed) = message.completed_at() else {
                continue;
            };
            if completed < from {
                page_has_old = true;
                continue;
            }
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            fetched += 1;
            collected.push(message);
        }
        cursor = page.next;
        if cursor.is_none() || page_has_old {
            break;
        }
    }
    collected.sort_by_key(|message| message.completed_at().unwrap_or(0));
    for message in collected {
        let parts = message.text_parts();
        if parts.is_empty() {
            continue;
        }
        let Some(body) = turn::recovered_body(session_id, &parts, settings, now) else {
            continue;
        };
        let item = LaneMsg::Backfill(BackfillItem {
            session_id: session_id.to_string(),
            topic: topic.clone(),
            message_id: message.id.clone(),
            body,
        });
        if out.send(item).await.is_err() {
            return Ok(fetched);
        }
    }
    Ok(fetched)
}

fn push_scope(scope: &mut Vec<String>, id: &str) {
    if id.is_empty() || scope.iter().any(|known| known == id) {
        return;
    }
    scope.push(id.to_string());
}
