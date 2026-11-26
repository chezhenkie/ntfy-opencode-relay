//! Event subscription and the push worker.
//!
//! The reader never awaits ntfy: it parses frames and hands them to a bounded
//! channel, dropping and counting when the worker is behind. The worker resolves
//! each session to its project topic, drives turn deadlines with one tick, and
//! publishes through `rust/crates/ntfy`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use ntfy_relay_core::asks::{Ask, AskStore, RegisterError, Registered};
use ntfy_relay_core::config::OutboundConfig;
use ntfy_relay_core::forms::FormStore;
use ntfy_relay_core::topic::{topic_for_project, ProjectRoots, TopicOwners};
use ntfy_relay_ntfy::header::Priority;
use ntfy_relay_ntfy::publish::Publisher;
use ntfy_relay_ntfy::sse::SseParser;
use ntfy_relay_opencode::api::Api;
use ntfy_relay_opencode::event::{parse as parse_event, Envelope, FileAction, RuntimeEvent};
use ntfy_relay_opencode::event_stream::EventSource;
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::files::{self, AutoAttachSettings};
use crate::format;
use crate::lane::{self, LaneMsg};
use crate::log::Log;
use crate::permission::{PendingNotice, PermissionScheduler};
use crate::publish::{self, OutboundFile, OutboundMessage};
use crate::seen::SeenEvents;
use crate::topic::{short_session_id, SessionMeta, SessionResolver};
use crate::turn::{TerminalMode, TurnBody, TurnOut, TurnSettings, TurnTracker};

pub const EVENT_TYPES: &[&str] = &[
    "session.created",
    "session.deleted",
    "session.status",
    "session.execution.started",
    "session.execution.succeeded",
    "session.execution.failed",
    "session.execution.interrupted",
    "session.text.ended",
    "filesystem.changed",
    "permission.asked",
    "permission.replied",
    "form.created",
];

pub const CHANNEL_CAPACITY: usize = 256;
/// A due ask survives a failing registry write for this long; the retry ramp
/// runs inside the budget.
pub const ASK_REGISTER_BUDGET_MS: i64 = 30_000;
/// Delivery attempts for a body push before its ids are left to backfill.
const PUBLISH_ATTEMPTS: u8 = 5;
/// Per-session warnings kept so one bad project cannot spam the log.
const MAX_WARNED: usize = 512;
const BACKOFF_START_MS: i64 = 1_000;
const BACKOFF_MAX_MS: i64 = 60_000;
const STABLE_CONNECTION: Duration = Duration::from_secs(60);
/// A 2xx stream that yields nothing for this long is dead; twice the ntfy
/// keepalive default of 45 s.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Reconnect backfill starts 60 s before the stream ended.
const BACKFILL_MARGIN_MS: i64 = 60_000;
/// Results a spawned lane task may hold before it waits for the worker.
const LANE_CAPACITY: usize = 64;

/// Connection generation and gap start, shared by the reader and the worker.
///
/// The reader marks every stream it opens; the worker turns the next unseen
/// generation into one backfill. Generation 1 is the process start.
#[derive(Default)]
pub struct FeedState {
    inner: std::sync::Mutex<FeedInner>,
}

#[derive(Default)]
struct FeedInner {
    generation: u64,
    gap_start_ms: i64,
    consumed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedResume {
    pub gap_start_ms: i64,
    pub cold: bool,
}

impl FeedState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called by the reader when a subscription returned a stream.
    pub fn subscribed(&self) {
        self.lock().generation += 1;
    }

    /// Called by the reader when a stream that was open ended.
    pub fn ended(&self, now: i64) {
        self.lock().gap_start_ms = now;
    }

    /// The newest start the worker has not handled yet. The worker commits the
    /// generation only after its backfill task has reported back.
    pub fn peek(&self) -> Option<(u64, FeedResume)> {
        let inner = self.lock();
        if inner.generation <= inner.consumed {
            return None;
        }
        Some((
            inner.generation,
            FeedResume {
                gap_start_ms: inner.gap_start_ms,
                cold: inner.generation == 1,
            },
        ))
    }

    /// Marks every generation up to `generation` as handled.
    pub fn commit(&self, generation: u64) {
        let mut inner = self.lock();
        if generation > inner.consumed {
            inner.consumed = generation;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FeedInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

#[derive(Debug, Clone)]
pub struct Timings {
    pub debounce_ms: i64,
    pub ttl_ms: i64,
    pub tick_ms: u64,
    /// How long a due ask survives a failing registry write before it is
    /// dropped: the retry ramp runs inside this budget.
    pub ask_register_budget_ms: i64,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            debounce_ms: crate::turn::TURN_DEBOUNCE_MS,
            ttl_ms: crate::turn::TURN_BUFFER_TTL_MS,
            tick_ms: 250,
            ask_register_budget_ms: ASK_REGISTER_BUDGET_MS,
        }
    }
}

/// Aborts a spawned helper when the owning task ends, including on unwind.
/// Without this, a panicking worker would leave its subscription task alive
/// and a supervisor restart would open a second stream.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> AbortOnDrop<T> {
    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

pub struct OutboundOptions {
    pub config: OutboundConfig,
    /// The live root list, shared with discovery; a control change applies to
    /// scope without a restart.
    pub roots: Arc<std::sync::RwLock<ProjectRoots>>,
    pub api: Arc<dyn Api>,
    pub events: Arc<dyn EventSource>,
    pub publisher: Publisher,
    pub log: Arc<dyn Log>,
    pub shutdown: watch::Receiver<bool>,
    pub timings: Timings,
    /// Connection generation shared with the reader; tests drive it directly.
    pub feed: Arc<FeedState>,
    /// Session ids the relay pins; always part of a backfill scope.
    pub focus: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    /// Journal of handled ids; dedupes replays and backfill after a restart.
    pub seen_file: Option<PathBuf>,
    /// Registry of published asks, shared with inbound reply handling.
    pub asks: Arc<AskStore>,
    /// Registry of pushed question forms, shared with inbound reply handling.
    pub forms: Arc<FormStore>,
    /// Topic ownership, shared with discovery and the inbound map.
    pub owners: Arc<std::sync::RwLock<TopicOwners>>,
}

pub struct RawEvent {
    pub id: String,
    pub location: Option<String>,
    pub event: RuntimeEvent,
}

pub async fn run(options: OutboundOptions) -> Result<(), String> {
    let OutboundOptions {
        config,
        roots,
        api,
        events,
        publisher,
        log,
        mut shutdown,
        timings,
        feed,
        focus,
        seen_file,
        asks,
        forms,
        owners,
    } = options;
    if !config.enabled {
        log.info("outbound disabled by config");
        return Ok(());
    }

    // With several roots the fallback is ambiguous, so the overflow notice
    // waits for a resolved last topic instead of picking the first root.
    let start_roots = roots
        .read()
        .map(|live| live.roots().to_vec())
        .unwrap_or_default();
    let fallback_topic = match start_roots.as_slice() {
        [root] => topic_for_project(&root.to_string_lossy()).map(str::to_string),
        _ => None,
    };
    let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
    let (lane_sender, mut lane_receiver) = mpsc::channel(LANE_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));
    let last_drop_log = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let mut reader = AbortOnDrop(tokio::spawn(subscribe_forever(
        events,
        log.clone(),
        sender,
        dropped.clone(),
        last_drop_log,
        feed.clone(),
        shutdown.clone(),
    )));

    let turn_settings = TurnSettings {
        inline_max_bytes: config.turn_text.inline_max_bytes,
        attachment_prefix: config.turn_text.attachment_prefix.clone(),
        debounce_ms: timings.debounce_ms,
        ttl_ms: timings.ttl_ms,
    };
    let mut worker = Worker {
        api,
        publisher,
        log: log.clone(),
        resolver: SessionResolver::new(roots, owners),
        turn_settings: turn_settings.clone(),
        tracker: TurnTracker::new(turn_settings, log.clone()),
        feed,
        focus,
        last_text_session: None,
        permissions: PermissionScheduler::new(config.permission_delay_ms as i64, log.clone()),
        ask_attempts: HashMap::new(),
        ask_budget_ms: timings.ask_register_budget_ms,
        retries: VecDeque::new(),
        lane: lane_sender,
        tasks: Vec::new(),
        backfill_active: None,
        backfill_published: 0,
        asks,
        forms,
        auto_attach: AutoAttachSettings {
            enabled: config.auto_attach.enabled,
            extensions: config.auto_attach.extensions.clone(),
            max_files: config.auto_attach.max_files,
            max_file_bytes: config.auto_attach.max_file_bytes,
            max_total_bytes: config.auto_attach.max_total_bytes,
            scan_max_entries: files::SCAN_MAX_ENTRIES,
        },
        events: config.events,
        suppress_subagents: config.suppress_subagents,
        dropped,
        overflow_seen: 0,
        overflow_notice_at: 0,
        last_topic: fallback_topic.clone(),
        fallback_topic,
        warned: HashSet::new(),
        warned_order: VecDeque::new(),
        seen: SeenEvents::open(seen_file, now_ms(), log.as_ref()),
    };

    let mut receiver = receiver;
    let mut tick = tokio::time::interval(Duration::from_millis(timings.tick_ms.max(1)));
    let mut unexpected_end = false;
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            message = receiver.recv() => {
                match message {
                    Some(raw) => worker.handle(raw, now_ms()).await,
                    None => {
                        // The reader may have returned on the shutdown signal
                        // just before this branch observed the watch change.
                        if !*shutdown.borrow() {
                            unexpected_end = true;
                        }
                        break;
                    }
                }
            }
            lane = lane_receiver.recv() => {
                if let Some(message) = lane {
                    worker.handle_lane(message, now_ms()).await;
                }
            }
            _ = tick.tick() => {
                worker.tick(now_ms()).await;
            }
        }
    }

    let queued = receiver.len();
    if queued > 0 {
        log.warn(&format!(
            "shutdown dropped {queued} queued outbound event(s)"
        ));
    }
    let turns = worker.tracker.clear();
    if turns > 0 {
        log.warn(&format!("shutdown dropped {turns} turn buffer(s)"));
    }
    let permissions = worker.permissions.clear();
    if permissions > 0 {
        log.warn(&format!(
            "shutdown dropped {permissions} pending permission notification(s)"
        ));
    }
    // A running lane task may be blocked in an API query or an upload; the
    // worker is the only owner, so shutdown cancels it instead of waiting.
    let tasks = worker.tasks.len();
    worker.tasks.clear();
    if tasks > 0 {
        log.warn(&format!("shutdown cancelled {tasks} outbound lane task(s)"));
    }
    drop(receiver);
    let _ = tokio::time::timeout(Duration::from_secs(5), &mut reader.0).await;
    log.info("outbound stopped");
    if unexpected_end {
        return Err("event subscription task ended".to_string());
    }
    Ok(())
}

struct Worker {
    api: Arc<dyn Api>,
    publisher: Publisher,
    log: Arc<dyn Log>,
    resolver: SessionResolver,
    turn_settings: TurnSettings,
    tracker: TurnTracker,
    feed: Arc<FeedState>,
    focus: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    last_text_session: Option<String>,
    permissions: PermissionScheduler,
    /// Persist retries per due request, bounded by `ask_budget_ms`.
    ask_attempts: HashMap<String, AskRetry>,
    ask_budget_ms: i64,
    /// Body pushes that failed once and still hold unseen message ids.
    retries: VecDeque<RetryPush>,
    /// Results from spawned lane tasks.
    lane: mpsc::Sender<LaneMsg>,
    /// Spawned backfill and auto-attach tasks; dropped tasks are aborted.
    tasks: Vec<AbortOnDrop<()>>,
    /// Feed generation whose backfill task is still running.
    backfill_active: Option<u64>,
    /// Messages the running backfill pass delivered on the first attempt.
    backfill_published: usize,
    asks: Arc<AskStore>,
    forms: Arc<FormStore>,
    auto_attach: AutoAttachSettings,
    events: ntfy_relay_core::config::OutboundEvents,
    suppress_subagents: ntfy_relay_core::config::SuppressSubagents,
    dropped: Arc<AtomicU64>,
    overflow_seen: u64,
    overflow_notice_at: i64,
    last_topic: Option<String>,
    fallback_topic: Option<String>,
    warned: HashSet<String>,
    warned_order: VecDeque<String>,
    seen: SeenEvents,
}

#[derive(Debug, Clone, Copy)]
struct AskRetry {
    attempts: u32,
    first_at: i64,
    next_at: i64,
}

/// 250 ms, 500 ms, 1 s, 2 s, then 4 s between registry write attempts.
fn ask_backoff_ms(attempts: u32) -> i64 {
    match attempts {
        0 => 250,
        1 => 500,
        2 => 1_000,
        3 => 2_000,
        _ => 4_000,
    }
}

impl Worker {
    /// The first delivery of an id wins; a duplicate from the other source is
    /// dropped. The id is journaled only after the effects have been attempted.
    /// A transient target lookup failure leaves the id unmarked so backfill can
    /// recover it.
    async fn handle(&mut self, raw: RawEvent, now: i64) {
        if !self.seen.accept(&raw.id, now) {
            return;
        }
        let id = raw.id.clone();
        if self.handle_event(raw, now).await {
            self.seen.forget(&id);
            return;
        }
        self.seen.commit(&id, now, self.log.as_ref()).await;
    }

    /// Returns true when the event must stay unmarked for a later recovery.
    async fn handle_event(&mut self, raw: RawEvent, now: i64) -> bool {
        match raw.event {
            RuntimeEvent::SessionCreated {
                session_id,
                parent_id,
            } => {
                self.resolver
                    .note_created(&session_id, parent_id.as_deref());
                if raw.location.is_some() {
                    self.target(&session_id, raw.location.as_deref(), now).await;
                }
                false
            }
            RuntimeEvent::SessionDeleted { session_id } => {
                self.resolver.forget(&session_id);
                self.tracker.cancel(&session_id);
                false
            }
            RuntimeEvent::TurnStarted { session_id } => {
                let outcome = self.target(&session_id, raw.location.as_deref(), now).await;
                let Some(_target) = outcome.target else {
                    return outcome.retryable;
                };
                let out = self.tracker.started(&session_id, now);
                self.publish_turns(out, now).await;
                false
            }
            RuntimeEvent::TextEnded {
                session_id,
                assistant_message_id,
                ordinal,
                text,
                time,
            } => {
                // A backfill or an earlier terminal may have delivered this
                // message already; a late part must not publish twice.
                if self.seen.contains(&assistant_message_id, now) {
                    return false;
                }
                let outcome = self.target(&session_id, raw.location.as_deref(), now).await;
                let Some(_target) = outcome.target else {
                    return outcome.retryable;
                };
                let out = self.tracker.append(
                    &session_id,
                    &assistant_message_id,
                    ordinal,
                    &text,
                    time,
                    now,
                );
                self.publish_turns(out, now).await;
                false
            }
            RuntimeEvent::FileChanged { action, path } => {
                if action == FileAction::Add {
                    self.tracker.add_file(&path);
                }
                false
            }
            RuntimeEvent::SessionIdle { session_id } => {
                if !self.events.session_idle {
                    // The notice is off, but its text still belongs on the
                    // phone; publish it as a partial before the early return.
                    let out = self.tracker.flush_partial(&session_id, now);
                    self.publish_turns(out, now).await;
                    return false;
                }
                let outcome = self.target(&session_id, raw.location.as_deref(), now).await;
                let Some(target) = outcome.target else {
                    return outcome.retryable;
                };
                if self.suppress_subagents.session_idle && target.is_child {
                    self.tracker.cancel(&session_id);
                    return false;
                }
                let out = self.tracker.terminal(&session_id, TerminalMode::Idle, now);
                self.publish_turns(out, now).await;
                false
            }
            RuntimeEvent::SessionError {
                session_id,
                message,
            } => {
                if !self.events.session_error {
                    let out = self.tracker.flush_partial(&session_id, now);
                    self.publish_turns(out, now).await;
                    return false;
                }
                let outcome = self.target(&session_id, raw.location.as_deref(), now).await;
                let Some(target) = outcome.target else {
                    return outcome.retryable;
                };
                if self.suppress_subagents.session_error && target.is_child {
                    self.tracker.cancel(&session_id);
                    return false;
                }
                let draft = format::error_message(&target.topic, Some(&session_id), &message);
                let _ = self.publish(&draft).await;
                let out = self.tracker.flush_partial(&session_id, now);
                self.publish_turns(out, now).await;
                false
            }
            RuntimeEvent::Interrupted { session_id, .. } => {
                let outcome = self.target(&session_id, raw.location.as_deref(), now).await;
                let Some(target) = outcome.target else {
                    return outcome.retryable;
                };
                if self.suppress_subagents.session_idle && target.is_child {
                    self.tracker.cancel(&session_id);
                    return false;
                }
                let out = self
                    .tracker
                    .terminal(&session_id, TerminalMode::Interrupted, now);
                self.publish_turns(out, now).await;
                false
            }
            RuntimeEvent::PermissionAsked {
                session_id,
                request_id,
                permission,
                patterns,
            } => {
                if !self.events.permission_asked {
                    return false;
                }
                let Some(session_id) = session_id else {
                    self.warn_once(
                        "permission-asked-without-session",
                        "permission push skipped: no session id",
                    );
                    return false;
                };
                let Some(request_id) = request_id else {
                    self.warn_once(
                        "permission-asked-without-request",
                        "permission push skipped: no request id",
                    );
                    return false;
                };
                let outcome = self.target(&session_id, raw.location.as_deref(), now).await;
                let Some(target) = outcome.target else {
                    return outcome.retryable;
                };
                let notice = PendingNotice {
                    session_id,
                    title: target.title,
                    topic: target.topic,
                    permission,
                    patterns,
                };
                if let Some(notice) = self.permissions.asked(&request_id, notice, now) {
                    self.publish_ask(&request_id, notice, now).await;
                }
                false
            }
            RuntimeEvent::PermissionReplied { request_id } => {
                self.permissions.replied(&request_id);
                if let Some(ask) = self.remove_ask(&request_id).await {
                    self.log.info(&format!(
                        "permission ask removed code={} reason=answered",
                        ask.code
                    ));
                }
                false
            }
            RuntimeEvent::FormCreated {
                session_id,
                form_id,
                title,
                kind,
                fields,
            } => {
                if !self.events.question_asked || kind.as_deref() != Some("question") {
                    return false;
                }
                let outcome = self.target(&session_id, raw.location.as_deref(), now).await;
                let Some(target) = outcome.target else {
                    return outcome.retryable;
                };
                // The record is written before the push, so `/answer` finds the
                // form it was shown. Every visible form can be dismissed from
                // the phone; the free text becomes the reason the asker reads.
                let visible: Vec<&ntfy_relay_opencode::event::FormField> =
                    fields.iter().filter(|field| !field.hidden).collect();
                if !visible.is_empty() {
                    if let Err(error) = self
                        .register_form(&session_id, &form_id, &target.topic, &title, &fields, now)
                        .await
                    {
                        self.log
                            .warn(&format!("question form not pushed id={form_id}: {error}"));
                        return false;
                    }
                }
                if let Some(message) =
                    format::question_message(&target.topic, &title, kind.as_deref(), &fields)
                {
                    let _ = self.publish(&message).await;
                }
                false
            }
        }
    }

    async fn tick(&mut self, now: i64) {
        self.retry_pending(now).await;
        self.start_backfill(now);
        self.tasks.retain(|task| !task.is_finished());
        let out = self.tracker.tick(now);
        self.publish_turns(out, now).await;
        for (request_id, notice) in self.permissions.tick(now) {
            self.publish_ask(&request_id, notice, now).await;
        }
        let total = self.dropped.load(Ordering::Relaxed);
        if total > self.overflow_seen && now - self.overflow_notice_at >= 60_000 {
            let dropped = total - self.overflow_seen;
            self.overflow_seen = total;
            self.overflow_notice_at = now;
            let topic = self
                .last_topic
                .clone()
                .or_else(|| self.fallback_topic.clone());
            if let Some(topic) = topic {
                let message = OutboundMessage {
                    kind: "outbound.overflow",
                    topic: topic.clone(),
                    title: format!("{topic} - outbound overflow"),
                    message: format!(
                        "{dropped} outbound event(s) were dropped because the queue was full."
                    ),
                    tags: vec!["warning", "oc-out"],
                    priority: Priority::High,
                    file: None,
                };
                let _ = self.publish(&message).await;
            }
        }
    }

    /// Starts one gap recovery when the reader opened a stream the worker has
    /// not answered yet. The fetch runs in a lane task; publishing stays here so
    /// one seen window and one turn tracker decide every duplicate.
    fn start_backfill(&mut self, now: i64) {
        if self.backfill_active.is_some() {
            return;
        }
        let Some((generation, resume)) = self.feed.peek() else {
            return;
        };
        let gap_ms = if resume.cold {
            0
        } else {
            (now - resume.gap_start_ms).max(0)
        };
        self.backfill_active = Some(generation);
        self.backfill_published = 0;
        let request = lane::BackfillRequest {
            generation,
            cold: resume.cold,
            from: backfill_from(resume, now),
            gap_ms,
            now,
            focus: (self.focus)(),
            open: self.tracker.open_sessions(),
            last_text: self.last_text_session.clone(),
        };
        let api = self.api.clone();
        let log = self.log.clone();
        let roots = self.resolver.roots_handle();
        let owners = self.resolver.owners_handle();
        let settings = self.turn_settings.clone();
        let out = self.lane.clone();
        self.tasks.push(AbortOnDrop(tokio::spawn(lane::backfill(
            request, api, log, roots, owners, settings, out,
        ))));
    }

    /// One result from a lane task. A recovered message is published here, not
    /// in the fetch task, so a live part and its recovered copy cannot both
    /// pass the seen window.
    async fn handle_lane(&mut self, message: LaneMsg, now: i64) {
        match message {
            LaneMsg::Backfill(item) => {
                if self.seen.contains(&item.message_id, now) {
                    self.tracker
                        .discard_message(&item.session_id, &item.message_id);
                    return;
                }
                self.seen.accept(&item.message_id, now);
                let push = body_message("session.backfill", &item.topic, item.body);
                let ids = vec![item.message_id.clone()];
                if self.deliver(push, ids, now).await {
                    self.backfill_published += 1;
                }
                self.tracker
                    .discard_message(&item.session_id, &item.message_id);
                self.last_text_session = Some(item.session_id);
            }
            LaneMsg::Done(done) => {
                self.feed.commit(done.generation);
                if self.backfill_active == Some(done.generation) {
                    self.backfill_active = None;
                }
                self.log.info(&format!(
                    "backfill cold={} gap={}ms sessions={} messages={} published={}",
                    done.cold, done.gap_ms, done.sessions, done.fetched, self.backfill_published
                ));
            }
            LaneMsg::Attached {
                topic,
                published,
                skipped,
                bytes,
            } => {
                if published > 0 || skipped > 0 {
                    self.log.info(&format!(
                        "auto-attach topic={topic} published={published} skipped={skipped} bytes={bytes}"
                    ));
                }
            }
        }
    }

    async fn publish_turns(&mut self, out: Vec<TurnOut>, now: i64) {
        for turn in out {
            let outcome = self.target(&turn.session_id, None, now).await;
            let Some(target) = outcome.target else {
                continue;
            };
            match turn.body {
                Some(body) => {
                    // Mark before the push, commit after it: a crash can repeat
                    // one push, a late part or backfill cannot publish twice.
                    // A failed push keeps the ids unseen and the body queued.
                    for message in &turn.messages {
                        self.seen.accept(message, now);
                    }
                    let message = body_message("session.text", &target.topic, body);
                    let message = if turn.partial {
                        let mut message = message;
                        message.kind = "session.text.partial";
                        message.tags.push("partial");
                        message
                    } else {
                        message
                    };
                    self.deliver(message, turn.messages, now).await;
                    self.last_text_session = Some(turn.session_id.clone());
                }
                None => {
                    if turn.idle {
                        let message = format::idle_message(&target.topic, &turn.session_id);
                        let _ = self.publish(&message).await;
                    }
                }
            }
            if let (Some(started_at), Some(root)) = (turn.started_at, target.canonical) {
                // The per-turn scan and uploads are slow; the lane task runs
                // them and reports the summary, so the worker stays free.
                if self.auto_attach.enabled && !turn.files.is_empty() {
                    let publisher = self.publisher.clone();
                    let log = self.log.clone();
                    let settings = self.auto_attach.clone();
                    let lane = self.lane.clone();
                    let topic = target.topic.clone();
                    let files = turn.files.clone();
                    self.tasks.push(AbortOnDrop(tokio::spawn(async move {
                        let stats = files::publish_auto_attached(
                            &publisher,
                            &topic,
                            &root,
                            &files,
                            started_at,
                            &settings,
                            log.as_ref(),
                        )
                        .await;
                        let _ = lane
                            .send(LaneMsg::Attached {
                                topic,
                                published: stats.published,
                                skipped: stats.skipped,
                                bytes: stats.bytes,
                            })
                            .await;
                    })));
                }
            }
        }
    }

    async fn target(
        &mut self,
        session_id: &str,
        location: Option<&str>,
        now: i64,
    ) -> TargetOutcome {
        let meta = self
            .resolver
            .meta(self.api.as_ref(), session_id, location, now)
            .await;
        let retryable = meta.retryable;
        TargetOutcome {
            target: self.target_from(session_id, meta),
            retryable,
        }
    }

    fn target_from(&mut self, session_id: &str, meta: SessionMeta) -> Option<SessionTarget> {
        if meta.denied {
            self.warn_once(
                &format!("denied:{session_id}"),
                &format!(
                    "outbound skipped session \"{session_id}\": project is outside projectRoots"
                ),
            );
            return None;
        }
        let is_child = meta.is_child();
        let canonical = meta.canonical;
        let Some(topic) = meta.topic else {
            self.warn_once(
                &format!("unresolved:{session_id}"),
                &format!("outbound skipped session \"{session_id}\": no project topic resolved"),
            );
            return None;
        };
        Some(SessionTarget {
            topic,
            title: meta.title.unwrap_or_else(|| short_session_id(session_id)),
            canonical,
            is_child,
        })
    }

    async fn publish(&mut self, message: &OutboundMessage) -> Result<(), String> {
        match publish::send(&self.publisher, message).await {
            Ok(()) => {
                self.last_topic = Some(message.topic.clone());
                Ok(())
            }
            Err(error) => {
                self.log.warn(&format!(
                    "outbound publish failed kind={} topic={}: {error}",
                    message.kind, message.topic
                ));
                Err(error.to_string())
            }
        }
    }

    /// Publishes a body push and commits its message ids only after the push
    /// succeeded. A failure keeps the ids unseen on disk and queues the body for
    /// one retry per tick; after five attempts the ids are forgotten so a later
    /// backfill can recover the text.
    async fn deliver(&mut self, message: OutboundMessage, messages: Vec<String>, now: i64) -> bool {
        match self.publish(&message).await {
            Ok(()) => {
                for id in &messages {
                    self.seen.commit(id, now, self.log.as_ref()).await;
                }
                true
            }
            Err(_) => {
                if !messages.is_empty() {
                    self.retries.push_back(RetryPush {
                        message,
                        messages,
                        attempts: 1,
                        next_at: now,
                    });
                }
                false
            }
        }
    }

    /// One retry attempt per tick keeps the push rate bounded while the phone
    /// is unreachable. The fifth failure drops the entry and leaves its ids
    /// unseen, so the next backfill pass may deliver the text again.
    async fn retry_pending(&mut self, now: i64) {
        let Some(front) = self.retries.front() else {
            return;
        };
        if front.next_at > now {
            return;
        }
        let Some(retry) = self.retries.pop_front() else {
            return;
        };
        let RetryPush {
            message,
            messages,
            attempts,
            ..
        } = retry;
        match self.publish(&message).await {
            Ok(()) => {
                for id in &messages {
                    self.seen.commit(id, now, self.log.as_ref()).await;
                }
            }
            Err(_) if attempts + 1 >= PUBLISH_ATTEMPTS => {
                for id in &messages {
                    self.seen.forget(id);
                }
                self.log.error(&format!(
                    "outbound publish dropped kind={} topic={} after {} attempts",
                    message.kind, message.topic, PUBLISH_ATTEMPTS
                ));
            }
            Err(_) => self.retries.push_back(RetryPush {
                message,
                messages,
                attempts: attempts + 1,
                next_at: now,
            }),
        }
    }

    /// Registers the ask before its push, publishes the code, and completes the
    /// scheduler entry. A persist failure keeps the entry pending and retries on
    /// a 250 ms to 4 s ramp inside a 30 s budget, then drops it with one error
    /// line. A publish failure removes the record.
    async fn publish_ask(&mut self, request_id: &str, notice: PendingNotice, now: i64) {
        if let Some(retry) = self.ask_attempts.get(request_id) {
            if retry.next_at > now {
                return;
            }
        }
        let registered = match self.register_ask(request_id, &notice, now).await {
            Ok(registered) => registered,
            Err(RegisterError::CodeCollision { .. }) => {
                self.permissions.complete(request_id);
                self.log.error(&format!(
                    "permission push dropped request={request_id} reason=code-collision"
                ));
                return;
            }
            Err(RegisterError::Io(error)) => {
                let (attempts, elapsed, delay, drop) = {
                    let retry =
                        self.ask_attempts
                            .entry(request_id.to_string())
                            .or_insert(AskRetry {
                                attempts: 0,
                                first_at: now,
                                next_at: now,
                            });
                    retry.attempts += 1;
                    let delay = ask_backoff_ms(retry.attempts - 1);
                    let elapsed = now - retry.first_at;
                    let drop = elapsed + delay > self.ask_budget_ms;
                    if !drop {
                        retry.next_at = now + delay;
                    }
                    (retry.attempts, elapsed, delay, drop)
                };
                self.log.warn(&format!(
                    "permission ask persist failed request={request_id} attempt={attempts} retry_in={delay}ms: {}",
                    clip(&error.to_string(), 200)
                ));
                if !drop {
                    return;
                }
                self.ask_attempts.remove(request_id);
                self.permissions.complete(request_id);
                self.log.error(&format!(
                    "permission push dropped request={request_id} reason=persist-failed after {elapsed} ms"
                ));
                return;
            }
        };
        self.ask_attempts.remove(request_id);
        if let Some(evicted) = registered.evicted {
            self.log.warn(&format!(
                "permission ask removed code={} reason=evicted",
                evicted.code
            ));
        }
        self.log.info(&format!(
            "permission ask code={} session={} request={request_id}",
            registered.code, notice.session_id
        ));
        let message = format::permission_message(
            &notice.topic,
            &notice.permission,
            &notice.patterns,
            Some(&notice.session_id),
            &registered.code,
        );
        if let Err(error) = publish::send(&self.publisher, &message).await {
            self.remove_ask(request_id).await;
            self.log.warn(&format!(
                "permission ask removed code={} reason=publish-failed: {error}",
                registered.code
            ));
        } else {
            self.last_topic = Some(notice.topic);
        }
        self.permissions.complete(request_id);
    }

    async fn register_ask(
        &self,
        request_id: &str,
        notice: &PendingNotice,
        now: i64,
    ) -> Result<Registered, RegisterError> {
        let asks = self.asks.clone();
        let request = request_id.to_string();
        let session = notice.session_id.clone();
        let topic = notice.topic.clone();
        let title = notice.title.clone();
        match tokio::task::spawn_blocking(move || {
            asks.register(&request, &session, &topic, &title, now)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => Err(RegisterError::Io(std::io::Error::other(error.to_string()))),
        }
    }

    async fn remove_ask(&self, request: &str) -> Option<Ask> {
        let asks = self.asks.clone();
        let request = request.to_string();
        tokio::task::spawn_blocking(move || asks.remove(&request))
            .await
            .ok()
            .flatten()
    }

    /// Registers the form before its push and returns the code the phone must
    /// name. Hidden fields are not registered and cannot be answered.
    async fn register_form(
        &self,
        session: &str,
        form: &str,
        topic: &str,
        title: &str,
        fields: &[ntfy_relay_opencode::event::FormField],
        now: i64,
    ) -> Result<String, String> {
        let fields: Vec<ntfy_relay_core::forms::FormField> = fields
            .iter()
            .filter(|field| !field.hidden)
            .map(|field| ntfy_relay_core::forms::FormField {
                key: field.key.clone(),
                title: field.title.clone(),
                kind: field.kind.clone(),
                required: field.required,
                multiple: field.multiple(),
                options: field
                    .options
                    .iter()
                    .map(|option| ntfy_relay_core::forms::FormOption {
                        label: option.label.clone(),
                        value: option.value.clone(),
                    })
                    .collect(),
            })
            .collect();
        let record = ntfy_relay_core::forms::FormRecord {
            code: String::new(),
            form: form.to_string(),
            session: session.to_string(),
            topic: topic.to_string(),
            title: title.to_string(),
            pushed: now,
            fields,
        };
        let store = self.forms.clone();
        match tokio::task::spawn_blocking(move || store.register(record)).await {
            Ok(Ok(registered)) => Ok(registered.code),
            Ok(Err(error)) => Err(error.to_string()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn warn_once(&mut self, key: &str, message: &str) {
        if !self.warned.insert(key.to_string()) {
            return;
        }
        self.warned_order.push_back(key.to_string());
        while self.warned_order.len() > MAX_WARNED {
            if let Some(oldest) = self.warned_order.pop_front() {
                self.warned.remove(&oldest);
            }
        }
        self.log.warn(message);
    }
}

struct TargetOutcome {
    target: Option<SessionTarget>,
    retryable: bool,
}

/// A body push whose first attempt failed. `attempts` counts the failed try
/// that queued it; the ids stay unseen until a retry is accepted.
struct RetryPush {
    message: OutboundMessage,
    messages: Vec<String>,
    attempts: u8,
    next_at: i64,
}

struct SessionTarget {
    topic: String,
    /// Session title at push time; falls back to the short session id.
    title: String,
    canonical: Option<PathBuf>,
    is_child: bool,
}

/// Reconnect backfill starts 60 s before the stream ended; a cold start uses
/// the seen-journal window.
fn backfill_from(resume: FeedResume, now: i64) -> i64 {
    if resume.cold {
        now - crate::seen::RETENTION_MS
    } else if resume.gap_start_ms > 0 {
        resume.gap_start_ms - BACKFILL_MARGIN_MS
    } else {
        now - BACKFILL_MARGIN_MS
    }
}

fn body_message(kind: &'static str, topic: &str, body: TurnBody) -> OutboundMessage {
    match body {
        TurnBody::Text(text) => OutboundMessage {
            kind,
            topic: topic.to_string(),
            title: topic.to_string(),
            message: text,
            tags: vec!["oc-out"],
            priority: Priority::Default,
            file: None,
        },
        TurnBody::Attachment {
            summary,
            body,
            filename,
        } => OutboundMessage {
            kind,
            topic: topic.to_string(),
            title: topic.to_string(),
            message: summary,
            tags: vec!["oc-out"],
            priority: Priority::Default,
            file: Some(OutboundFile {
                filename,
                content_type: "text/markdown; charset=utf-8".to_string(),
                data: bytes::Bytes::from(body),
            }),
        },
    }
}

async fn subscribe_forever(
    events: Arc<dyn EventSource>,
    log: Arc<dyn Log>,
    sender: mpsc::Sender<RawEvent>,
    dropped: Arc<AtomicU64>,
    last_drop_log: Arc<std::sync::atomic::AtomicI64>,
    feed: Arc<FeedState>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = BACKOFF_START_MS;
    let mut first = true;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let result = tokio::select! {
            result = events.subscribe(EVENT_TYPES) => result,
            _ = shutdown.changed() => return,
        };
        let stream = match result {
            Ok(stream) => stream,
            Err(error) => {
                let status = error.code();
                if matches!(status, Some(code) if (400..500).contains(&code) && code != 401 && code != 429)
                {
                    log.error(&format!(
                        "event subscribe failed: {}; retrying in 60 s",
                        clip(error.message(), 300)
                    ));
                    if wait_or_stop(&mut shutdown, 60_000).await {
                        return;
                    }
                    continue;
                }
                let wait = jitter(backoff);
                log.warn(&format!(
                    "event subscribe failed: {}; retry in {wait} ms",
                    clip(error.message(), 300)
                ));
                backoff = (backoff * 2).min(BACKOFF_MAX_MS);
                if wait_or_stop(&mut shutdown, wait).await {
                    return;
                }
                continue;
            }
        };
        if first {
            log.info("event stream subscribed");
            first = false;
        } else {
            log.warn("event stream gap: backfill covers completed text only");
            log.info("event stream subscribed");
        }
        feed.subscribed();
        let connected = Instant::now();
        let mut parser = SseParser::new();
        let mut overflow_logged = false;
        let mut chunks = stream.chunks;
        let idle = tokio::time::sleep(STREAM_IDLE_TIMEOUT);
        tokio::pin!(idle);
        let failure = loop {
            tokio::select! {
                chunk = chunks.next() => {
                    idle.as_mut().reset(tokio::time::Instant::now() + STREAM_IDLE_TIMEOUT);
                    match chunk {
                    Some(Ok(bytes)) => {
                        let events = parser.push(&bytes);
                        if let Some(kind) = parser.take_overflow() {
                            if !overflow_logged {
                                log.warn(&format!(
                                    "event frame dropped: {} cap exceeded",
                                    overflow_name(kind)
                                ));
                                overflow_logged = true;
                            }
                        }
                        for value in events {
                            forward(value, &sender, &dropped, &last_drop_log, log.as_ref()).await;
                        }
                    }
                    Some(Err(error)) => break error.message().to_string(),
                    None => {
                        let events = parser.finish();
                        if let Some(kind) = parser.take_overflow() {
                            if !overflow_logged {
                                log.warn(&format!(
                                    "event frame dropped: {} cap exceeded",
                                    overflow_name(kind)
                                ));
                            }
                        }
                        for value in events {
                            forward(value, &sender, &dropped, &last_drop_log, log.as_ref()).await;
                        }
                        break "event stream ended".to_string();
                    }
                    }
                },
                _ = &mut idle => {
                    break format!(
                        "event stream idle for {} s",
                        STREAM_IDLE_TIMEOUT.as_secs()
                    );
                }
                _ = shutdown.changed() => return,
            }
        };
        if connected.elapsed() >= STABLE_CONNECTION {
            backoff = BACKOFF_START_MS;
        }
        feed.ended(now_ms());
        let wait = jitter(backoff);
        log.warn(&format!(
            "event stream failed: {}; retry in {wait} ms",
            clip(&failure, 300)
        ));
        backoff = (backoff * 2).min(BACKOFF_MAX_MS);
        if wait_or_stop(&mut shutdown, wait).await {
            return;
        }
    }
}

fn overflow_name(kind: ntfy_relay_ntfy::sse::SseOverflow) -> &'static str {
    match kind {
        ntfy_relay_ntfy::sse::SseOverflow::Line => "line",
        ntfy_relay_ntfy::sse::SseOverflow::Data => "data",
    }
}

pub(crate) async fn forward(
    value: Value,
    sender: &mpsc::Sender<RawEvent>,
    dropped: &AtomicU64,
    last_drop_log: &std::sync::atomic::AtomicI64,
    log: &dyn Log,
) {
    let envelope = match Envelope::parse(&value) {
        Some(envelope) => envelope,
        None => return,
    };
    let Some(event) = parse_event(&value) else {
        if ntfy_relay_opencode::event::missing_text_message_id(&value) {
            log.debug("event skipped: session.text.ended without assistantMessageID");
        }
        return;
    };
    let raw = RawEvent {
        id: envelope.id,
        location: envelope.location,
        event,
    };
    if control_event(&raw.event) {
        // An ask, a form push or an error must not be lost to a text flood.
        // It waits for the worker to free a slot instead of dropping.
        let _ = sender.send(raw).await;
        return;
    }
    match sender.try_send(raw) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            let total = dropped.fetch_add(1, Ordering::Relaxed) + 1;
            let now = now_ms();
            let last = last_drop_log.load(Ordering::Relaxed);
            if total == 1 || now - last >= 60_000 {
                last_drop_log.store(now, Ordering::Relaxed);
                log.warn(&format!(
                    "outbound queue full; dropping events (total {total})"
                ));
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}

fn control_event(event: &RuntimeEvent) -> bool {
    matches!(
        event,
        RuntimeEvent::PermissionAsked { .. }
            | RuntimeEvent::FormCreated { .. }
            | RuntimeEvent::SessionError { .. }
    )
}

async fn wait_or_stop(shutdown: &mut watch::Receiver<bool>, millis: i64) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(millis.max(1) as u64)) => false,
        _ = shutdown.changed() => true,
    }
}

fn jitter(ms: i64) -> i64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    let factor = 0.8 + (nanos as f64 / 1_000_000_000.0) * 0.4;
    (ms as f64 * factor).round() as i64
}

pub(crate) fn clip(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.to_string()
    } else {
        let mut out: String = value.chars().take(limit).collect();
        out.push_str("...");
        out
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_util::stream::BoxStream;
    use ntfy_relay_core::asks::Resolve;
    use ntfy_relay_core::config::{
        AutoAttachConfig, OutboundEvents, SuppressSubagents, TurnTextConfig,
    };
    use ntfy_relay_ntfy::http::{HttpError, HttpRequest, HttpResponse};
    use ntfy_relay_ntfy::publish::NtfyServer;
    use ntfy_relay_opencode::api::{
        ApiError, CreateSession, Delivery, MessageContent, MessageInfo, MessagePage, MessageTime,
        PermissionInfo, ProjectInfo, PromptFile, ReplyDecision, SessionInfo,
    };
    use ntfy_relay_opencode::event_stream::EventStream;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Mutex;
    use tokio::sync::mpsc::UnboundedSender;

    /// One project (`p1`) rooted at `<temp>/proj`; backfill tests add sessions
    /// and message pages. Live tests resolve through the envelope location.
    #[derive(Default)]
    struct TestApi {
        projects: Mutex<Vec<ProjectInfo>>,
        sessions: Mutex<HashMap<String, Vec<SessionInfo>>>,
        by_id: Mutex<HashMap<String, SessionInfo>>,
        pages: Mutex<HashMap<String, Vec<Vec<MessageInfo>>>>,
        fail_messages: Mutex<bool>,
        gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
        gated: std::sync::atomic::AtomicBool,
    }

    impl TestApi {
        fn add_session(&self, session_id: &str) {
            self.add_session_with_parent(session_id, None);
        }

        fn add_session_with_parent(&self, session_id: &str, parent_id: Option<&str>) {
            let info = SessionInfo {
                id: session_id.to_string(),
                project_id: Some("p1".to_string()),
                parent_id: parent_id.map(str::to_string),
                title: None,
                time: Default::default(),
            };
            self.by_id
                .lock()
                .unwrap()
                .insert(session_id.to_string(), info.clone());
            self.sessions
                .lock()
                .unwrap()
                .entry("p1".to_string())
                .or_default()
                .push(info);
        }

        fn set_messages(&self, session_id: &str, messages: Vec<MessageInfo>) {
            self.pages
                .lock()
                .unwrap()
                .insert(session_id.to_string(), vec![messages]);
        }

        fn set_pages(&self, session_id: &str, pages: Vec<Vec<MessageInfo>>) {
            self.pages
                .lock()
                .unwrap()
                .insert(session_id.to_string(), pages);
        }

        fn fail_message_queries(&self) {
            *self.fail_messages.lock().unwrap() = true;
        }

        /// Holds every message query until the returned gate is notified.
        fn gate_messages(&self) -> Arc<tokio::sync::Notify> {
            let gate = Arc::new(tokio::sync::Notify::new());
            *self.gate.lock().unwrap() = Some(gate.clone());
            gate
        }
    }

    #[async_trait]
    impl Api for TestApi {
        async fn projects(&self) -> Result<Vec<ProjectInfo>, ApiError> {
            Ok(self.projects.lock().unwrap().clone())
        }

        async fn sessions(
            &self,
            project_id: &str,
            limit: usize,
        ) -> Result<Vec<SessionInfo>, ApiError> {
            Ok(self
                .sessions
                .lock()
                .unwrap()
                .get(project_id)
                .map(|sessions| sessions.iter().take(limit).cloned().collect())
                .unwrap_or_default())
        }

        async fn session(&self, id: &str) -> Result<SessionInfo, ApiError> {
            self.by_id
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| ApiError::with_status(404, "not found"))
        }

        async fn active_ids(&self) -> Result<Vec<String>, ApiError> {
            Ok(Vec::new())
        }

        async fn messages(
            &self,
            session_id: &str,
            _limit: usize,
            cursor: Option<&str>,
        ) -> Result<MessagePage, ApiError> {
            if *self.fail_messages.lock().unwrap() {
                return Err(ApiError::with_status(500, "messages unavailable"));
            }
            let gate = self.gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                self.gated.store(true, Ordering::Relaxed);
                gate.notified().await;
            }
            let index = cursor
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            let pages = self.pages.lock().unwrap();
            let page = pages
                .get(session_id)
                .and_then(|pages| pages.get(index))
                .cloned()
                .unwrap_or_default();
            let total = pages.get(session_id).map(|pages| pages.len()).unwrap_or(0);
            let next = (index + 1 < total).then(|| (index + 1).to_string());
            Ok(MessagePage {
                messages: page,
                next,
            })
        }

        async fn create_session(
            &self,
            _request: CreateSession<'_>,
        ) -> Result<SessionInfo, ApiError> {
            unimplemented!()
        }

        async fn prompt(
            &self,
            _session_id: &str,
            _text: &str,
            _delivery: Delivery,
            _files: &[PromptFile],
        ) -> Result<(), ApiError> {
            unimplemented!()
        }

        async fn permissions(&self, _session_id: &str) -> Result<Vec<PermissionInfo>, ApiError> {
            unimplemented!()
        }

        async fn reply_permission(
            &self,
            _session_id: &str,
            _permission_id: &str,
            _decision: ReplyDecision,
        ) -> Result<(), ApiError> {
            unimplemented!()
        }

        async fn cancel_form(
            &self,
            _session_id: &str,
            _form_id: &str,
            _message: &str,
        ) -> Result<(), ApiError> {
            unimplemented!()
        }
    }

    type Chunk = Result<Bytes, ApiError>;

    struct FakeSource {
        receiver: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Chunk>>>,
        opens: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl EventSource for FakeSource {
        async fn subscribe(&self, _types: &[&str]) -> Result<EventStream, ApiError> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            let receiver = self
                .receiver
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| ApiError::new("test stream exhausted"))?;
            let stream: BoxStream<'static, Chunk> =
                futures_util::stream::unfold(receiver, |mut receiver| async move {
                    receiver.recv().await.map(|item| (item, receiver))
                })
                .boxed();
            Ok(EventStream {
                status: 200,
                headers: Vec::new(),
                chunks: stream,
            })
        }
    }

    #[derive(Default)]
    struct Recorder {
        requests: Mutex<Vec<HttpRequest>>,
        status: std::sync::atomic::AtomicU16,
        fail_next: std::sync::atomic::AtomicUsize,
    }

    impl Recorder {
        fn fail(&self) {
            self.status.store(500, Ordering::Relaxed);
        }

        /// The next request answers 500, later ones answer 200.
        fn fail_next(&self) {
            self.fail_next.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[async_trait]
    impl ntfy_relay_ntfy::http::Transport for Recorder {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
            self.requests.lock().unwrap().push(request);
            let one_shot = self
                .fail_next
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    left.checked_sub(1)
                })
                .is_ok();
            let status = if one_shot {
                500
            } else {
                match self.status.load(Ordering::Relaxed) {
                    0 => 200,
                    status => status,
                }
            };
            Ok(HttpResponse {
                status,
                headers: Vec::new(),
                body: Bytes::new(),
            })
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("outbound-runner-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proj")).unwrap();
        root
    }

    fn config(delay_ms: u64) -> OutboundConfig {
        OutboundConfig {
            enabled: true,
            events: OutboundEvents {
                session_idle: true,
                session_error: true,
                permission_asked: true,
                question_asked: true,
            },
            suppress_subagents: SuppressSubagents {
                session_idle: true,
                session_error: true,
            },
            permission_delay_ms: delay_ms,
            turn_text: TurnTextConfig {
                inline_max_bytes: 3400,
                attachment_prefix: "turn".to_string(),
            },
            auto_attach: AutoAttachConfig {
                enabled: false,
                extensions: Vec::new(),
                max_files: 8,
                max_file_bytes: 10_485_760,
                max_total_bytes: 31_457_280,
            },
        }
    }

    #[derive(Default)]
    struct TestLog {
        lines: Mutex<Vec<String>>,
    }

    impl TestLog {
        fn warnings(&self) -> Vec<String> {
            self.lines
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.starts_with("warn:"))
                .cloned()
                .collect()
        }

        fn errors(&self) -> Vec<String> {
            self.lines
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.starts_with("error:"))
                .cloned()
                .collect()
        }
    }

    impl Log for TestLog {
        fn debug(&self, message: &str) {
            self.lines.lock().unwrap().push(format!("debug:{message}"));
        }
        fn info(&self, message: &str) {
            self.lines.lock().unwrap().push(format!("info:{message}"));
        }
        fn warn(&self, message: &str) {
            self.lines.lock().unwrap().push(format!("warn:{message}"));
        }
        fn error(&self, message: &str) {
            self.lines.lock().unwrap().push(format!("error:{message}"));
        }
    }

    struct Harness {
        root: PathBuf,
        transport: Arc<Recorder>,
        sender: UnboundedSender<Chunk>,
        shutdown: watch::Sender<bool>,
        handle: tokio::task::JoinHandle<Result<(), String>>,
        seen: Option<PathBuf>,
        api: Arc<TestApi>,
        feed: Arc<FeedState>,
        focus: Arc<Mutex<Vec<String>>>,
        log: Arc<TestLog>,
        asks: Arc<AskStore>,
        forms: Arc<FormStore>,
    }

    fn harness(name: &str, delay_ms: u64) -> Harness {
        harness_with(name, delay_ms, |_| {})
    }

    fn harness_with(name: &str, delay_ms: u64, setup: impl FnOnce(&TestApi)) -> Harness {
        harness_at_with(temp_root(name), delay_ms, setup)
    }

    fn harness_at_with(root: PathBuf, delay_ms: u64, setup: impl FnOnce(&TestApi)) -> Harness {
        harness_full(root, delay_ms, None, setup)
    }

    fn harness_at_asks(root: PathBuf, delay_ms: u64, asks_path: PathBuf) -> Harness {
        harness_full(root, delay_ms, Some(asks_path), |_| {})
    }

    fn harness_full(
        root: PathBuf,
        delay_ms: u64,
        asks_path: Option<PathBuf>,
        setup: impl FnOnce(&TestApi),
    ) -> Harness {
        harness_config(root, config(delay_ms), asks_path, setup)
    }

    fn harness_events(name: &str, events: OutboundEvents) -> Harness {
        let mut cfg = config(0);
        cfg.events = events;
        harness_config(temp_root(name), cfg, None, |_| {})
    }

    fn harness_config(
        root: PathBuf,
        cfg: OutboundConfig,
        asks_path: Option<PathBuf>,
        setup: impl FnOnce(&TestApi),
    ) -> Harness {
        let transport = Arc::new(Recorder::default());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let source = Arc::new(FakeSource {
            receiver: Mutex::new(Some(receiver)),
            opens: std::sync::atomic::AtomicUsize::new(0),
        });
        let publisher = Publisher::new(
            NtfyServer::parse("https://ntfy.test").unwrap(),
            Some("token-1234567890".to_string()),
            transport.clone(),
        );
        let (shutdown, shutdown_rx) = watch::channel(false);
        let seen = Some(root.join(crate::seen::SEEN_FILE));
        let api = Arc::new(TestApi::default());
        api.projects.lock().unwrap().push(ProjectInfo {
            id: "p1".to_string(),
            canonical: Some(root.join("proj").to_string_lossy().into_owned()),
            time: Default::default(),
        });
        setup(&api);
        let feed = Arc::new(FeedState::new());
        let focus_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let focus = {
            let ids = focus_ids.clone();
            Arc::new(move || ids.lock().unwrap().clone())
        };
        let log = Arc::new(TestLog::default());
        let mut owner_map = TopicOwners::new();
        owner_map.insert("proj", root.join("proj"));
        let owners = Arc::new(std::sync::RwLock::new(owner_map));
        let (ask_store, _) = AskStore::open(
            asks_path.unwrap_or_else(|| root.join(ntfy_relay_core::asks::ASK_FILE)),
            now_ms(),
        );
        let asks = Arc::new(ask_store);
        let (form_store, _) = ntfy_relay_core::forms::FormStore::open(
            root.join(ntfy_relay_core::forms::FORM_FILE),
            now_ms(),
        );
        let forms = Arc::new(form_store);
        let options = OutboundOptions {
            config: cfg,
            roots: Arc::new(std::sync::RwLock::new(ProjectRoots::new(
                vec![root.clone()],
            ))),
            api: api.clone(),
            events: source,
            publisher,
            log: log.clone(),
            shutdown: shutdown_rx,
            timings: Timings {
                debounce_ms: 20,
                ttl_ms: 1000,
                tick_ms: 5,
                ask_register_budget_ms: 120,
            },
            feed: feed.clone(),
            focus,
            seen_file: seen.clone(),
            asks: asks.clone(),
            forms: forms.clone(),
            owners,
        };
        let handle = tokio::spawn(run(options));
        Harness {
            root,
            transport,
            sender,
            shutdown,
            handle,
            seen,
            api,
            feed,
            focus: focus_ids,
            log,
            asks,
            forms,
        }
    }

    impl Harness {
        fn frame(&self, value: Value) {
            let line = format!("data: {value}\n\n");
            self.sender
                .send(Ok(Bytes::from(line)))
                .expect("test stream open");
        }

        async fn wait_requests(&self, count: usize) -> Vec<HttpRequest> {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let requests = self.transport.requests.lock().unwrap().clone();
                    if requests.len() >= count {
                        return requests;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("publish timeout")
        }

        async fn wait_warning(&self, needle: &str) {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if self.log.warnings().iter().any(|line| line.contains(needle)) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("warning timeout: {needle}"));
        }

        async fn stop(self) -> Result<(), String> {
            self.shutdown.send(true).unwrap();
            self.handle.await.unwrap()
        }
    }

    fn session_created(root: &Path, session: &str, parent: Option<&str>) -> Value {
        let mut data = json!({
            "sessionID": session,
            "projectID": "p1",
            "location": { "directory": root.join("proj").to_string_lossy() }
        });
        if let Some(parent) = parent {
            data["parentID"] = json!(parent);
        }
        json!({
            "id": format!("evt_{session}"),
            "created": 1_794_400_000_000i64,
            "type": "session.created",
            "location": { "directory": root.join("proj").to_string_lossy() },
            "data": data
        })
    }

    fn json_body(request: &HttpRequest) -> Value {
        serde_json::from_slice(&request.body).unwrap()
    }

    #[tokio::test]
    async fn a_turn_publishes_one_link_safe_text_push() {
        let harness = harness("text", 0);
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "session.execution.started",
            "data": { "sessionID": "ses_a" }
        }));
        harness.frame(json!({
            "id": "e3", "type": "session.text.ended",
            "created": 1_794_400_000_100i64,
            "data": {
                "sessionID": "ses_a",
                "assistantMessageID": "msg_1",
                "ordinal": 0,
                "text": "done readme.md"
            }
        }));
        harness.frame(json!({
            "id": "e4", "type": "session.execution.succeeded",
            "data": { "sessionID": "ses_a" }
        }));
        let requests = harness.wait_requests(1).await;
        let body = json_body(&requests[0]);
        assert_eq!(body["topic"], "proj");
        assert_eq!(body["title"], "proj");
        assert_eq!(body["message"], "done readme[.]md");
        assert_eq!(body["priority"], 3);
        assert_eq!(body["tags"], json!(["oc-out"]));
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_replied_permission_never_reaches_the_phone() {
        let harness = harness("permission-cancel", 30);
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "permission.asked",
            "data": { "id": "per_1", "sessionID": "ses_a", "action": "bash", "resources": ["git status"] }
        }));
        tokio::time::sleep(Duration::from_millis(5)).await;
        harness.frame(json!({
            "id": "e3", "type": "permission.replied",
            "data": { "requestID": "per_1" }
        }));
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(harness.transport.requests.lock().unwrap().is_empty());
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn an_unanswered_permission_registers_the_ask_before_its_push() {
        let harness = harness("permission-send", 20);
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "permission.asked",
            "data": { "id": "per_1", "sessionID": "ses_a", "action": "bash", "resources": ["git status"] }
        }));
        let requests = harness.wait_requests(1).await;
        let body = json_body(&requests[0]);
        let code = match harness.asks.resolve(None, "proj") {
            Resolve::Found(ask) => {
                assert_eq!(ask.request, "per_1");
                assert_eq!(ask.session, "ses_a");
                ask.code
            }
            other => panic!("expected a registered ask, got {other:?}"),
        };
        assert_eq!(body["title"], "proj - permission requested");
        assert_eq!(
            body["message"],
            format!(
                "Permission \"bash\" is requested.\nSession: ses_a\nPatterns: git status\nReply /allow {code}, /always {code}, or /deny {code}; with one ask, /allow is enough."
            )
        );
        assert_eq!(body["tags"], json!(["lock", "oc-out"]));
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_zero_delay_ask_is_registered_too() {
        let harness = harness("permission-zero", 0);
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "permission.asked",
            "data": { "id": "per_1", "sessionID": "ses_a", "action": "bash", "resources": [] }
        }));
        harness.wait_requests(1).await;
        assert_eq!(harness.asks.count(), 1);
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_desktop_answer_removes_the_ask_and_keeps_a_tombstone() {
        let harness = harness("permission-answered", 0);
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "permission.asked",
            "data": { "id": "per_1", "sessionID": "ses_a", "action": "bash", "resources": [] }
        }));
        harness.wait_requests(1).await;
        assert_eq!(harness.asks.count(), 1);
        harness.frame(json!({
            "id": "e3", "type": "permission.replied",
            "data": { "requestID": "per_1" }
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(harness.asks.count(), 0);
        assert!(
            harness.asks.answered("per_1").is_some(),
            "a desktop answer leaves a tombstone for a late phone reply"
        );
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_publish_removes_the_ask() {
        let harness = harness("permission-publish-fail", 20);
        harness.transport.fail();
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "permission.asked",
            "data": { "id": "per_1", "sessionID": "ses_a", "action": "bash", "resources": [] }
        }));
        harness.wait_requests(1).await;
        harness.wait_warning("reason=publish-failed").await;
        assert_eq!(harness.asks.count(), 0);
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_persist_failure_past_the_budget_drops_the_ask_without_a_push() {
        let root = temp_root("permission-persist");
        // A regular file where the store expects a directory: the read, the
        // quarantine and the persist all fail, so the ask never reaches the wire.
        let blocked = root.join("asks-blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let harness = harness_at_asks(root, 20, blocked.join("ntfy-relay-asks.json"));
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "permission.asked",
            "data": { "id": "per_1", "sessionID": "ses_a", "action": "bash", "resources": [] }
        }));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            harness.transport.requests.lock().unwrap().is_empty(),
            "a persist failure suppresses the push"
        );
        assert!(
            harness
                .log
                .warnings()
                .iter()
                .any(|line| line.contains("permission ask persist failed")),
            "{:?}",
            harness.log.warnings()
        );
        assert!(
            harness
                .log
                .errors()
                .iter()
                .any(|line| line
                    .contains("permission push dropped request=per_1 reason=persist-failed")),
            "{:?}",
            harness.log.errors()
        );
        harness.stop().await.unwrap();
    }

    #[test]
    fn the_ask_retry_ramp_reaches_four_seconds() {
        let ramp: Vec<i64> = (0..6).map(ask_backoff_ms).collect();
        assert_eq!(ramp, vec![250, 500, 1_000, 2_000, 4_000, 4_000]);
    }

    #[tokio::test]
    async fn a_question_form_publishes_the_question_text() {
        let harness = harness("question", 0);
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "form.created",
            "data": { "form": {
                "id": "frm_1",
                "sessionID": "ses_a",
                "title": "Questions",
                "metadata": { "kind": "question" },
                "fields": [{
                    "key": "q0",
                    "title": "Scope",
                    "description": "Pick one",
                    "type": "string",
                    "options": [{ "value": "a", "label": "A", "description": "first" }]
                }]
            }}
        }));
        let requests = harness.wait_requests(1).await;
        let body = json_body(&requests[0]);
        assert_eq!(body["title"], "proj - question");
        let message = body["message"].as_str().unwrap();
        assert!(message.starts_with("Questions\nScope: Pick one\n  - A (first)\n"));
        assert_eq!(
            message.lines().last().unwrap(),
            "Reply with /answer and your answer in your own words."
        );
        assert!(
            matches!(
                harness.forms.resolve(None, "proj"),
                ntfy_relay_core::forms::Resolve::Found(_)
            ),
            "the form is registered before its push"
        );
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_child_session_idle_is_suppressed() {
        let harness = harness("child", 0);
        harness.frame(session_created(
            &harness.root,
            "ses_child",
            Some("ses_root"),
        ));
        harness.frame(json!({
            "id": "e2", "type": "session.execution.succeeded",
            "data": { "sessionID": "ses_child" }
        }));
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(harness.transport.requests.lock().unwrap().is_empty());
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn an_idle_without_text_still_notifies() {
        let harness = harness("idle", 0);
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "session.execution.succeeded",
            "data": { "sessionID": "ses_a" }
        }));
        let requests = harness.wait_requests(1).await;
        let body = json_body(&requests[0]);
        assert_eq!(body["title"], "proj - session idle");
        assert_eq!(
            body["message"],
            "Session ses_a has finished its run and is idle.\nIt is waiting for your next instruction."
        );
        harness.stop().await.unwrap();
    }

    fn message(id: &str, completed: i64, parts: &[&str]) -> MessageInfo {
        MessageInfo {
            id: id.to_string(),
            time: MessageTime {
                created: completed - 10,
                completed: Some(completed),
            },
            content: parts
                .iter()
                .map(|text| MessageContent {
                    kind: "text".to_string(),
                    text: Some((*text).to_string()),
                })
                .collect(),
        }
    }

    #[test]
    fn backfill_windows_start_before_the_gap_and_use_the_journal_on_cold_start() {
        let now = 1_000_000;
        assert_eq!(
            backfill_from(
                FeedResume {
                    gap_start_ms: 500_000,
                    cold: false
                },
                now
            ),
            440_000
        );
        assert_eq!(
            backfill_from(
                FeedResume {
                    gap_start_ms: 0,
                    cold: false
                },
                now
            ),
            now - 60_000
        );
        assert_eq!(
            backfill_from(
                FeedResume {
                    gap_start_ms: 0,
                    cold: true
                },
                now
            ),
            now - crate::seen::RETENTION_MS
        );
    }

    #[tokio::test]
    async fn a_cold_start_backfills_only_the_journal_window() {
        let harness = harness_with("backfill-cold", 0, |api| {
            api.add_session("ses_a");
            let now = now_ms();
            api.set_messages(
                "ses_a",
                vec![
                    message("msg_fresh", now - 5_000, &["fresh text"]),
                    message("msg_old", now - 20 * 60 * 1000, &["old text"]),
                ],
            );
        });
        let requests = harness.wait_requests(1).await;
        let body = json_body(&requests[0]);
        assert_eq!(body["message"], "fresh text");
        assert_eq!(body["topic"], "proj");
        assert_eq!(body["tags"], json!(["oc-out"]));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            harness.transport.requests.lock().unwrap().len(),
            1,
            "text older than the seen window is not recovered"
        );
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_reconnect_backfills_missed_messages_in_order_once() {
        let harness = harness("backfill-reconnect", 0);
        tokio::time::sleep(Duration::from_millis(50)).await;
        harness.api.add_session("ses_a");
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e-start", "type": "session.execution.started",
            "data": { "sessionID": "ses_a" }
        }));
        let now = now_ms();
        harness.api.set_pages(
            "ses_a",
            vec![
                vec![message("msg_2", now - 2_000, &["part two"])],
                vec![message("msg_1", now - 9_000, &["part one"])],
            ],
        );
        harness.focus.lock().unwrap().push("ses_a".to_string());
        harness.feed.ended(now_ms());
        harness.feed.subscribed();
        let requests = harness.wait_requests(2).await;
        assert_eq!(json_body(&requests[0])["message"], "part one");
        assert_eq!(json_body(&requests[1])["message"], "part two");

        harness.frame(json!({
            "id": "e-late", "type": "session.text.ended",
            "data": {
                "sessionID": "ses_a",
                "assistantMessageID": "msg_1",
                "ordinal": 0,
                "text": "part one"
            }
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            harness.transport.requests.lock().unwrap().len(),
            2,
            "a late live part of a backfilled message is dropped"
        );
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_published_turn_blocks_a_later_backfill() {
        let harness = harness("backfill-live-first", 0);
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "session.execution.started",
            "data": { "sessionID": "ses_a" }
        }));
        harness.frame(json!({
            "id": "e3", "type": "session.text.ended",
            "data": {
                "sessionID": "ses_a",
                "assistantMessageID": "msg_live",
                "ordinal": 0,
                "text": "live text"
            }
        }));
        harness.frame(json!({
            "id": "e4", "type": "session.execution.succeeded",
            "data": { "sessionID": "ses_a" }
        }));
        let requests = harness.wait_requests(1).await;
        assert_eq!(json_body(&requests[0])["message"], "live text");

        harness.api.add_session("ses_a");
        harness.api.set_messages(
            "ses_a",
            vec![message("msg_live", now_ms() - 1_000, &["live text"])],
        );
        harness.feed.ended(now_ms());
        harness.feed.subscribed();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            harness.transport.requests.lock().unwrap().len(),
            1,
            "the delivered message id is in the journal"
        );
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn buffered_parts_are_dropped_after_backfill_delivers_the_message() {
        let harness = harness("backfill-buffered", 0);
        harness.api.add_session("ses_a");
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "session.execution.started",
            "data": { "sessionID": "ses_a" }
        }));
        harness.frame(json!({
            "id": "e3", "type": "session.text.ended",
            "data": {
                "sessionID": "ses_a", "assistantMessageID": "msg_1",
                "ordinal": 0, "text": "part one"
            }
        }));
        harness.frame(json!({
            "id": "e4", "type": "session.text.ended",
            "data": {
                "sessionID": "ses_a", "assistantMessageID": "msg_1",
                "ordinal": 1, "text": "part two"
            }
        }));
        tokio::time::sleep(Duration::from_millis(300)).await;
        harness.api.set_messages(
            "ses_a",
            vec![message("msg_1", now_ms(), &["part one", "part two"])],
        );
        harness.feed.ended(now_ms());
        harness.feed.subscribed();
        let requests = harness.wait_requests(1).await;
        assert_eq!(json_body(&requests[0])["message"], "part one\n\npart two");
        harness.frame(json!({
            "id": "e5", "type": "session.execution.succeeded",
            "data": { "sessionID": "ses_a" }
        }));
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            harness.transport.requests.lock().unwrap().len(),
            1,
            "the terminal must not resend or push an idle notice"
        );
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_child_session_is_not_backfilled() {
        let harness = harness_with("backfill-child", 0, |api| {
            api.add_session_with_parent("ses_child", Some("ses_root"));
            api.set_messages(
                "ses_child",
                vec![message("msg_1", now_ms() - 1_000, &["child text"])],
            );
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(harness.transport.requests.lock().unwrap().is_empty());
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_backfill_query_keeps_the_live_stream() {
        let harness = harness_with("backfill-failure", 0, |api| {
            api.add_session("ses_a");
            api.fail_message_queries();
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            harness
                .log
                .warnings()
                .iter()
                .any(|line| line.contains("backfill session=ses_a failed")),
            "the query failure is logged"
        );

        harness.frame(session_created(&harness.root, "ses_b", None));
        harness.frame(json!({
            "id": "e2", "type": "session.execution.started",
            "data": { "sessionID": "ses_b" }
        }));
        harness.frame(json!({
            "id": "e3", "type": "session.text.ended",
            "data": {
                "sessionID": "ses_b", "assistantMessageID": "msg_live",
                "ordinal": 0, "text": "still live"
            }
        }));
        harness.frame(json!({
            "id": "e4", "type": "session.execution.succeeded",
            "data": { "sessionID": "ses_b" }
        }));
        let requests = harness.wait_requests(1).await;
        assert_eq!(json_body(&requests[0])["message"], "still live");
        harness.stop().await.unwrap();
    }

    #[test]
    fn the_subscription_asks_for_the_mapped_types_only() {
        assert!(EVENT_TYPES.contains(&"form.created"));
        assert!(EVENT_TYPES.contains(&"session.text.ended"));
        assert!(!EVENT_TYPES.contains(&"session.text.delta"));
        assert!(!EVENT_TYPES.contains(&"question.asked"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_event_stream_reconnects_after_the_idle_deadline() {
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        let (_chunk_sender, chunk_receiver) = tokio::sync::mpsc::unbounded_channel();
        let source = Arc::new(FakeSource {
            receiver: Mutex::new(Some(chunk_receiver)),
            opens: std::sync::atomic::AtomicUsize::new(0),
        });
        drop(receiver);
        let log = Arc::new(TestLog::default());
        let feed = Arc::new(FeedState::new());
        let (shutdown, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(subscribe_forever(
            source.clone(),
            log.clone(),
            sender,
            Arc::new(AtomicU64::new(0)),
            Arc::new(std::sync::atomic::AtomicI64::new(0)),
            feed,
            shutdown_rx,
        ));
        for _ in 0..200 {
            if source.opens.load(Ordering::Relaxed) >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(source.opens.load(Ordering::Relaxed), 1);
        tokio::time::advance(STREAM_IDLE_TIMEOUT + Duration::from_secs(1)).await;
        for _ in 0..20 {
            if source.opens.load(Ordering::Relaxed) >= 2 {
                break;
            }
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert!(source.opens.load(Ordering::Relaxed) >= 2);
        assert!(
            log.warnings()
                .iter()
                .any(|line| line.contains("event stream idle for 90 s")),
            "{:?}",
            log.warnings()
        );
        shutdown.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn a_message_in_the_seen_journal_is_not_backfilled_after_a_restart() {
        let root = temp_root("seen-restart");
        let first = harness_at_with(root.clone(), 0, |api| {
            api.add_session("ses_a");
            api.set_messages(
                "ses_a",
                vec![message("msg_restart", now_ms() - 1_000, &["restart text"])],
            );
        });
        let requests = first.wait_requests(1).await;
        assert_eq!(json_body(&requests[0])["message"], "restart text");
        let journal = first.seen.clone().unwrap();
        first.stop().await.unwrap();
        assert!(
            std::fs::read_to_string(&journal)
                .unwrap_or_default()
                .contains("msg_restart"),
            "backfilled message ids are journaled"
        );

        let second = harness_at_with(root, 0, |api| {
            api.add_session("ses_a");
            api.set_messages(
                "ses_a",
                vec![message("msg_restart", now_ms() - 1_000, &["restart text"])],
            );
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            second.transport.requests.lock().unwrap().is_empty(),
            "a journaled message must not publish again after a restart"
        );
        second.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_publish_is_retried_and_journaled_only_after_success() {
        let harness = harness("publish-retry", 0);
        harness.transport.fail_next();
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "session.execution.started",
            "data": { "sessionID": "ses_a" }
        }));
        harness.frame(json!({
            "id": "e3", "type": "session.text.ended",
            "data": {
                "sessionID": "ses_a",
                "assistantMessageID": "msg_retry",
                "ordinal": 0,
                "text": "retried text"
            }
        }));
        harness.frame(json!({
            "id": "e4", "type": "session.execution.succeeded",
            "data": { "sessionID": "ses_a" }
        }));
        let requests = harness.wait_requests(2).await;
        assert_eq!(requests.len(), 2);
        assert_eq!(json_body(&requests[0])["message"], "retried text");
        assert_eq!(json_body(&requests[1])["message"], "retried text");
        assert!(
            harness
                .log
                .warnings()
                .iter()
                .any(|line| line.contains("outbound publish failed kind=session.text")),
            "{:?}",
            harness.log.warnings()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let journal = std::fs::read_to_string(harness.seen.as_ref().unwrap()).unwrap_or_default();
        assert!(
            journal.contains("msg_retry"),
            "the id is committed only after the retry succeeded: {journal}"
        );
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_publish_that_keeps_failing_stops_after_five_attempts() {
        let harness = harness("publish-give-up", 0);
        harness.transport.fail();
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "session.execution.started",
            "data": { "sessionID": "ses_a" }
        }));
        harness.frame(json!({
            "id": "e3", "type": "session.text.ended",
            "data": {
                "sessionID": "ses_a",
                "assistantMessageID": "msg_give_up",
                "ordinal": 0,
                "text": "lost text"
            }
        }));
        harness.frame(json!({
            "id": "e4", "type": "session.execution.succeeded",
            "data": { "sessionID": "ses_a" }
        }));
        let requests = harness.wait_requests(PUBLISH_ATTEMPTS as usize).await;
        assert_eq!(requests.len(), PUBLISH_ATTEMPTS as usize);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            harness.transport.requests.lock().unwrap().len(),
            PUBLISH_ATTEMPTS as usize,
            "a dropped push is not retried again"
        );
        assert!(
            harness.log.errors().iter().any(|line| {
                line.contains("outbound publish dropped kind=session.text")
                    && line.contains("after 5 attempts")
            }),
            "{:?}",
            harness.log.errors()
        );
        let journal = std::fs::read_to_string(harness.seen.as_ref().unwrap()).unwrap_or_default();
        assert!(
            !journal.contains("msg_give_up"),
            "a dropped push leaves its id unseen for a later backfill: {journal}"
        );
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_slow_backfill_does_not_block_a_permission_ask() {
        let harness = harness_with("lane-permission", 0, |api| {
            api.add_session("ses_a");
            api.add_session("ses_p");
            api.set_messages(
                "ses_a",
                vec![message("msg_slow", now_ms() - 1_000, &["slow text"])],
            );
            api.gate_messages();
        });
        let gate = harness.api.gate.lock().unwrap().clone().unwrap();
        for _ in 0..100 {
            if harness.api.gated.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        harness.frame(session_created(&harness.root, "ses_p", None));
        harness.frame(json!({
            "id": "e2", "type": "permission.asked",
            "data": { "id": "per_p", "sessionID": "ses_p", "action": "bash", "resources": [] }
        }));
        let requests = harness.wait_requests(1).await;
        assert_eq!(
            json_body(&requests[0])["title"],
            "proj - permission requested",
            "the ask goes out while the backfill query is still gated"
        );
        gate.notify_one();
        let requests = harness.wait_requests(2).await;
        assert_eq!(json_body(&requests[1])["message"], "slow text");
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_control_event_waits_for_a_slot_instead_of_dropping() {
        let (sender, mut receiver) = mpsc::channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let last = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let log = Arc::new(TestLog::default());
        forward(
            json!({
                "id": "e1", "type": "session.text.ended",
                "data": {
                    "sessionID": "s", "assistantMessageID": "m",
                    "ordinal": 0, "text": "x"
                }
            }),
            &sender,
            &dropped,
            &last,
            log.as_ref(),
        )
        .await;
        assert_eq!(dropped.load(Ordering::Relaxed), 0);

        let tx = sender.clone();
        let task_dropped = dropped.clone();
        let task_last = last.clone();
        let task_log = log.clone();
        let blocked = tokio::spawn(async move {
            forward(
                json!({
                    "id": "e2", "type": "permission.asked",
                    "data": { "id": "p", "sessionID": "s", "action": "bash", "resources": [] }
                }),
                &tx,
                &task_dropped,
                &task_last,
                task_log.as_ref(),
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !blocked.is_finished(),
            "a control event waits for a slot instead of dropping"
        );
        let first = receiver.recv().await.expect("text event");
        assert!(matches!(first.event, RuntimeEvent::TextEnded { .. }));
        let second = receiver.recv().await.expect("control event");
        assert!(matches!(second.event, RuntimeEvent::PermissionAsked { .. }));
        blocked.await.unwrap();
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn shutdown_cancels_a_running_backfill() {
        let harness = harness_with("lane-shutdown", 0, |api| {
            api.add_session("ses_a");
            api.set_messages(
                "ses_a",
                vec![message("msg_blocked", now_ms() - 1_000, &["blocked text"])],
            );
            api.gate_messages();
        });
        for _ in 0..100 {
            if harness.api.gated.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            harness.api.gated.load(Ordering::Relaxed),
            "the backfill reached the gated query"
        );
        let result = tokio::time::timeout(Duration::from_secs(2), harness.stop()).await;
        assert!(result.is_ok(), "shutdown was held by the lane task");
        result.unwrap().unwrap();
    }

    #[tokio::test]
    async fn an_idle_with_the_notice_off_flushes_the_buffer() {
        let mut events = config(0).events;
        events.session_idle = false;
        let harness = harness_events("notice-idle-off", events);
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "session.execution.started",
            "data": { "sessionID": "ses_a" }
        }));
        harness.frame(json!({
            "id": "e3", "type": "session.text.ended",
            "data": {
                "sessionID": "ses_a", "assistantMessageID": "msg_1",
                "ordinal": 0, "text": "first text"
            }
        }));
        harness.frame(json!({
            "id": "e4", "type": "session.execution.started",
            "data": { "sessionID": "ses_a" }
        }));
        let requests = harness.wait_requests(1).await;
        let body = json_body(&requests[0]);
        assert_eq!(body["message"], "first text");
        assert_eq!(body["tags"], json!(["oc-out", "partial"]));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            harness.transport.requests.lock().unwrap().len(),
            1,
            "the buffer is published once"
        );
        harness.stop().await.unwrap();
    }

    #[tokio::test]
    async fn both_notices_off_still_publishes_the_buffer_at_the_ttl() {
        let mut events = config(0).events;
        events.session_idle = false;
        events.session_error = false;
        let harness = harness_events("notice-both-off", events);
        harness.frame(session_created(&harness.root, "ses_a", None));
        harness.frame(json!({
            "id": "e2", "type": "session.execution.started",
            "data": { "sessionID": "ses_a" }
        }));
        harness.frame(json!({
            "id": "e3", "type": "session.text.ended",
            "data": {
                "sessionID": "ses_a", "assistantMessageID": "msg_ttl",
                "ordinal": 0, "text": "kept text"
            }
        }));
        let requests = harness.wait_requests(1).await;
        let body = json_body(&requests[0]);
        assert_eq!(body["message"], "kept text");
        assert_eq!(body["tags"], json!(["oc-out", "partial"]));
        harness.stop().await.unwrap();
    }
}
