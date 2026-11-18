//! Composition root: config, lock, logger, adapter, discovery, and the main
//! loop that drains the inbound queue.
//!
//! Mirrors `main` and `Relay.run` in `ntfy-inbox-relay.mjs`, with the SSE
//! reader in `sse.rs` and the per-message work in `relay.rs`. There is one
//! process and one state writer.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use ntfy_relay_core::adapter::{PermissionApi, SessionApi};
use ntfy_relay_core::asks::{prune_corrupt_backups as prune_ask_backups, AskStore, ASK_FILE};
use ntfy_relay_core::clock::Clock;
use ntfy_relay_core::config::{default_config_path, home_dir, install_root, load_config, Config};
use ntfy_relay_core::state::{
    prune_corrupt_backups, quarantine_state, read_state_report, State, StateError,
    CORRUPT_BACKUP_TTL_SECS,
};
use ntfy_relay_core::topic::TopicOwners;
use ntfy_relay_inbound::control::engine::{Control, Status};
use ntfy_relay_inbound::control::load::{default_control_path, load_control};
use ntfy_relay_inbound::prune::{prune_inboxes, INBOX_PRUNE_INTERVAL_MS, INBOX_RETENTION_MS};
use ntfy_relay_inbound::queue::MessageQueue;
use ntfy_relay_ntfy::http::{ReqwestTransport, Transport};
use ntfy_relay_ntfy::publish::{NtfyServer, Publisher};
use ntfy_relay_ntfy::stream::StreamTransport;
use ntfy_relay_opencode::adapter::OpenCodeAdapter;
use ntfy_relay_opencode::api::{Api, ModelRef};
use ntfy_relay_opencode::event_stream::EventSource;
use ntfy_relay_outbound::runner::{run as run_outbound, FeedState, OutboundOptions};
use ntfy_relay_outbound::seen::SEEN_FILE;
use tokio::sync::{watch, Mutex, Notify};

use crate::api_client::{ApiFactory, RediscoveringApi};
use crate::discovery::{build_project_map, ProjectMap};
use crate::lock::{acquire_lock, lock_path_for, Acquired};
use crate::logger::{Logger, MAX_LOG_BYTES};
use crate::relay::Relay;
use crate::sse::SseTask;
use crate::store::StateStore;
use crate::supervisor::{self, RestartPolicy, TASK_STOP_GRACE};
use crate::util::{clip, now_ms};

struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        self.origin.elapsed().as_millis() as i64
    }
}

/// `NTFY_RELAY_CONFIG`, else `<install root>/ntfy-relay.json`, where the root
/// is `NTFY_RELAY_HOME` or the directory that holds the running binary.
pub fn config_path() -> PathBuf {
    if let Some(value) = std::env::var_os("NTFY_RELAY_CONFIG") {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    match install_root() {
        Some(root) => default_config_path(&root),
        None => PathBuf::from("ntfy-relay.json"),
    }
}

pub async fn run(config_path: PathBuf) -> Result<(), String> {
    let config = load_config(&config_path).map_err(|error| error.to_string())?;
    let logger = Logger::new(
        Some(config.logging.file.clone()),
        config.logging.level,
        config.logging.echo,
        MAX_LOG_BYTES,
    );
    logger.init().map_err(|error| {
        format!(
            "log: cannot open {}: {error}",
            config.logging.file.display()
        )
    })?;

    if !config.enabled {
        logger.info("relay disabled by config, exiting");
        return Ok(());
    }

    let root =
        install_root().ok_or_else(|| "config: cannot determine the install root".to_string())?;
    let loaded_control = match load_control(&default_control_path(&root)) {
        Ok(loaded) => loaded,
        Err(error) => {
            logger.error(&error.to_string());
            return Err(error.to_string());
        }
    };
    if loaded_control.missing {
        logger.warn(&format!(
            "control: {} not found, using built-in actions",
            loaded_control.path.display()
        ));
    }

    let lock_file = lock_path_for(&config.state_file);
    let lock = match acquire_lock(&lock_file, std::process::id(), now_ms()) {
        Ok(Acquired::Yes) => LockGuard::new(&lock_file, std::process::id()),
        Ok(Acquired::Running(pid)) => {
            logger.info(&format!("relay already running with pid {pid}, exiting"));
            return Ok(());
        }
        Ok(outcome) => {
            logger.error(&format!(
                "cannot acquire lock {}: {outcome}; another process may hold it",
                lock_file.display()
            ));
            return Err(format!("cannot acquire lock {}", lock_file.display()));
        }
        Err(error) => {
            logger.error(&format!(
                "cannot acquire lock {}: {error}",
                lock_file.display()
            ));
            return Err(error.to_string());
        }
    };

    let token = match fs::read_to_string(&config.ntfy.token_file) {
        Ok(token) => token.trim().to_string(),
        Err(error) => {
            logger.error(&format!(
                "cannot read token file {}: {error}",
                config.ntfy.token_file.display()
            ));
            return Err(error.to_string());
        }
    };
    if token.len() < 10 {
        logger.error(&format!(
            "token file {} is empty or too short",
            config.ntfy.token_file.display()
        ));
        return Err("token file is empty or too short".to_string());
    }

    let state = match read_state_report(&config.state_file) {
        Ok((state, report)) => {
            if report.truncated_ids > 0 || report.dropped_topics > 0 {
                logger.warn(&format!(
                    "state trimmed ids={} topics={}",
                    report.truncated_ids, report.dropped_topics
                ));
            }
            state
        }
        Err(StateError::Invalid(_)) | Err(StateError::Version(_)) => {
            match quarantine_state(&config.state_file, now_ms()) {
                Ok(target) => {
                    logger.warn(&format!(
                        "state quarantined to {}, starting empty",
                        target.display()
                    ));
                    State::new()
                }
                Err(error) => {
                    logger.error(&format!("state quarantine failed: {error}"));
                    return Err(error.to_string());
                }
            }
        }
        Err(error) => {
            logger.error(&error.to_string());
            return Err(error.to_string());
        }
    };

    match prune_corrupt_backups(&config.state_file, now_ms(), CORRUPT_BACKUP_TTL_SECS) {
        Ok(0) => {}
        Ok(removed) => logger.warn(&format!("state pruned corrupt backups={removed}")),
        Err(error) => logger.warn(&format!("state backup prune failed: {error}")),
    };

    let asks_path = config
        .state_file
        .parent()
        .map(|parent| parent.join(ASK_FILE))
        .unwrap_or_else(|| PathBuf::from(ASK_FILE));
    let (asks, asks_report) = AskStore::open(asks_path.clone(), now_ms());
    if asks_report.trimmed > 0 {
        logger.warn(&format!(
            "asks trimmed rows={} loaded={}",
            asks_report.trimmed, asks_report.loaded
        ));
    }
    if let Some(path) = asks_report.quarantined {
        logger.warn(&format!(
            "asks quarantined to {}, starting empty",
            path.display()
        ));
    }
    if let Some(error) = asks_report.quarantine_error {
        logger.warn(&format!("asks quarantine failed: {error}; starting empty"));
    }
    if let Some(error) = asks_report.read_error {
        logger.warn(&format!("asks read failed: {error}; starting empty"));
    }
    match prune_ask_backups(&asks_path, now_ms(), CORRUPT_BACKUP_TTL_SECS) {
        Ok(0) => {}
        Ok(removed) => logger.warn(&format!("asks pruned corrupt backups={removed}")),
        Err(error) => logger.warn(&format!("asks backup prune failed: {error}")),
    }
    let asks = Arc::new(asks);

    let store = Arc::new(StateStore::new(
        config.state_file.clone(),
        state,
        logger.clone(),
    ));
    let home =
        home_dir().ok_or_else(|| "config: cannot determine the home directory".to_string())?;
    let api = build_api(&config, &home, logger.clone()).inspect_err(|error| logger.error(error))?;
    let adapter = build_adapter(&config, api.clone());
    let permissions: Arc<dyn PermissionApi> = adapter.clone();
    let adapter_api: Arc<dyn SessionApi> = adapter.clone();

    let server = NtfyServer::parse(&config.ntfy.server).map_err(|error| {
        let message = format!("config: ntfy.server: {error}");
        logger.error(&message);
        message
    })?;
    let transport_impl = Arc::new(ReqwestTransport::new().map_err(|error| {
        let message = error.to_string();
        logger.error(&message);
        message
    })?);
    let transport: Arc<dyn Transport> = transport_impl.clone();
    let stream: Arc<dyn StreamTransport> = transport_impl;
    let publisher = Publisher::new(server.clone(), Some(token.clone()), transport.clone());

    let log_name = config_path.display().to_string();
    let roots = config
        .project_roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(",");
    logger.info(&format!(
        "relay starting config={log_name} roots={roots} outbound={}",
        config.outbound.enabled
    ));

    let (topics_tx, topics_rx) = watch::channel(Vec::new());
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let queue = Arc::new(Mutex::new(MessageQueue::new()));
    let notify = Arc::new(Notify::new());
    let queue_len = Arc::new(AtomicUsize::new(0));
    let last_processed = Arc::new(AtomicI64::new(0));
    let last_event_at = Arc::new(AtomicI64::new(0));
    let topics_count = Arc::new(AtomicUsize::new(0));
    let stopping = Arc::new(AtomicBool::new(false));
    let owners: Arc<RwLock<TopicOwners>> = Arc::new(RwLock::new(TopicOwners::new()));

    // Discovery runs before the outbound task so the shared topic ownership is
    // populated; an empty map owns nothing and would deny every publish.
    let mut projects = ProjectMap::default();
    let mut warning_signature = String::new();
    let mut detail_signature = String::new();
    let mut previous_keys: Vec<String> = Vec::new();
    discover_once(
        api.as_ref(),
        &config,
        &logger,
        &mut projects,
        &owners,
        &topics_tx,
        &topics_count,
        &mut previous_keys,
        &mut warning_signature,
        &mut detail_signature,
    )
    .await;

    let mut outbound_supervisor = if config.outbound.enabled {
        let outbound_api: Arc<dyn Api> = api.clone();
        let outbound_events: Arc<dyn EventSource> = api.clone();
        let outbound_config = config.outbound.clone();
        let outbound_roots = config.project_roots.clone();
        let outbound_server = server.clone();
        let outbound_token = token.clone();
        let outbound_transport = transport.clone();
        let outbound_logger = logger.clone();
        let outbound_shutdown = shutdown_rx.clone();
        let outbound_owners = owners.clone();
        let outbound_asks = asks.clone();
        let seen_file = config
            .state_file
            .parent()
            .map(|parent| parent.join(SEEN_FILE))
            .unwrap_or_else(|| PathBuf::from(SEEN_FILE));
        let outbound_feed = Arc::new(FeedState::new());
        let outbound_focus = {
            let store = store.clone();
            Arc::new(move || store.tracked_sessions())
        };
        Some(supervisor::spawn(
            "outbound",
            logger.clone(),
            shutdown_rx.clone(),
            RestartPolicy::default(),
            move || {
                run_outbound(OutboundOptions {
                    config: outbound_config.clone(),
                    project_roots: outbound_roots.clone(),
                    api: outbound_api.clone(),
                    events: outbound_events.clone(),
                    publisher: Publisher::new(
                        outbound_server.clone(),
                        Some(outbound_token.clone()),
                        outbound_transport.clone(),
                    ),
                    log: Arc::new(outbound_logger.clone()),
                    shutdown: outbound_shutdown.clone(),
                    timings: Default::default(),
                    feed: outbound_feed.clone(),
                    focus: outbound_focus.clone(),
                    seen_file: Some(seen_file.clone()),
                    asks: outbound_asks.clone(),
                    owners: outbound_owners.clone(),
                })
            },
        ))
    } else {
        logger.info("outbound disabled by config");
        None
    };

    let sse_server = server.clone();
    let sse_token = token.clone();
    let sse_replay_window = config.ntfy.replay_window_minutes;
    let sse_logger = logger.clone();
    let sse_transport = stream;
    let sse_queue = queue.clone();
    let sse_notify = notify.clone();
    let sse_queue_len = queue_len.clone();
    let sse_last_processed = last_processed.clone();
    let sse_last_event_at = last_event_at.clone();
    let sse_topics = topics_rx;
    let sse_shutdown = shutdown_rx.clone();

    prune_now(&config, &logger).await;

    let mut sse_supervisor = Some(supervisor::spawn(
        "inbound sse",
        logger.clone(),
        shutdown_rx.clone(),
        RestartPolicy::default(),
        move || {
            let task = SseTask {
                server: sse_server.clone(),
                token: sse_token.clone(),
                replay_window_minutes: sse_replay_window,
                logger: sse_logger.clone(),
                transport: sse_transport.clone(),
                queue: sse_queue.clone(),
                notify: sse_notify.clone(),
                queue_len: sse_queue_len.clone(),
                last_processed: sse_last_processed.clone(),
                last_event_at: sse_last_event_at.clone(),
            };
            let topics = sse_topics.clone();
            let shutdown = sse_shutdown.clone();
            async move {
                task.run(topics, shutdown).await;
                Ok::<(), String>(())
            }
        },
    ));

    let signal_logger = logger.clone();
    let signal_shutdown = shutdown_tx.clone();
    let signal_stopping = stopping.clone();
    let signal_handle = tokio::spawn(async move {
        let signal = wait_for_signal().await;
        signal_logger.info(&format!("received {signal}, stopping"));
        signal_stopping.store(true, Ordering::Relaxed);
        let _ = signal_shutdown.send(true);
    });

    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let start = Instant::now();
    let status_store = store.clone();
    let status_topics = topics_count.clone();
    let status_queue = queue_len.clone();
    let status_asks = asks.clone();
    let status = move || Status {
        topics: status_topics.load(Ordering::Relaxed),
        queue: status_queue.load(Ordering::Relaxed),
        target: Some(status_store.target_summary()),
        asks: status_asks.count(),
        uptime_seconds: Some(start.elapsed().as_secs()),
    };
    let target_store = store.clone();
    let hint_now = || now_ms();
    let mut control = Control::new(loaded_control.table, adapter.as_ref(), clock.as_ref())
        .with_status(&status)
        .with_targets(target_store.as_ref())
        .with_hint_clock(&hint_now);
    for warning in control.disabled_warnings() {
        logger.warn(&warning);
    }

    let relay = Relay::new(
        config.clone(),
        logger.clone(),
        store,
        adapter_api,
        permissions,
        asks,
        publisher,
        transport,
        last_processed,
        server,
        root.join("ntfy-deploy.json"),
    );

    let mut discovery_tick = tokio::time::interval(Duration::from_secs(60));
    discovery_tick.tick().await;
    let mut prune_tick =
        tokio::time::interval(Duration::from_millis(INBOX_PRUNE_INTERVAL_MS as u64));
    prune_tick.tick().await;
    let mut heartbeat_tick = tokio::time::interval(Duration::from_secs_f64(
        config.logging.heartbeat_minutes * 60.0,
    ));
    heartbeat_tick.tick().await;

    let mut sse_done = false;
    let mut outbound_done = false;
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            result = async {
                sse_supervisor
                    .as_mut()
                    .expect("sse supervisor is active")
                    .await
            }, if !sse_done => {
                sse_done = true;
                if *shutdown_rx.borrow() {
                    break;
                }
                stopping.store(true, Ordering::Relaxed);
                logger.error(&format!("inbound sse supervisor ended: {result:?}"));
                break;
            }
            result = async {
                outbound_supervisor
                    .as_mut()
                    .expect("outbound supervisor is active")
                    .await
            }, if outbound_supervisor.is_some() && !outbound_done => {
                outbound_done = true;
                if *shutdown_rx.borrow() {
                    break;
                }
                stopping.store(true, Ordering::Relaxed);
                logger.error(&format!("outbound supervisor ended: {result:?}"));
                break;
            }
            _ = notify.notified() => {
                relay.drain(&queue, &queue_len, &projects, &mut control, &stopping, &notify).await;
            }
            _ = discovery_tick.tick() => {
                discover_once(
                    api.as_ref(),
                    &config,
                    &logger,
                    &mut projects,
                    &owners,
                    &topics_tx,
                    &topics_count,
                    &mut previous_keys,
                    &mut warning_signature,
                    &mut detail_signature,
                ).await;
            }
            _ = prune_tick.tick() => prune_now(&config, &logger).await,
            _ = heartbeat_tick.tick() => {
                let last = last_event_at.load(Ordering::Relaxed);
                let age = if last > 0 { (now_ms() - last) / 1000 } else { -1 };
                logger.info(&format!(
                    "heartbeat topics={} lastEventAge={} queue={}",
                    topics_count.load(Ordering::Relaxed),
                    age,
                    queue_len.load(Ordering::Relaxed)
                ));
            }
        }
    }

    let _ = shutdown_tx.send(true);
    stopping.store(true, Ordering::Relaxed);
    let stop_wait = TASK_STOP_GRACE + Duration::from_secs(1);
    if !outbound_done {
        if let Some(handle) = outbound_supervisor.take() {
            let _ = tokio::time::timeout(stop_wait, handle).await;
        }
    }
    if !sse_done {
        if let Some(handle) = sse_supervisor.take() {
            let _ = tokio::time::timeout(stop_wait, handle).await;
        }
    }
    signal_handle.abort();
    let dropped = {
        let mut queue = queue.lock().await;
        queue.clear()
    };
    if dropped > 0 {
        logger.warn(&format!("shutdown dropped {dropped} queued message(s)"));
    }
    relay.flush().await;
    logger.info("relay stopped");
    logger.flush();
    drop(lock);
    Ok(())
}

/// Registration is read once here and again by the wrapper after a transport
/// error or 401, so a service restart does not leave the daemon on a dead port.
fn build_api(
    config: &Config,
    home: &std::path::Path,
    logger: Logger,
) -> Result<Arc<RediscoveringApi>, String> {
    let source = ApiFactory::from_config(config, home)?;
    RediscoveringApi::new(Arc::new(source), logger).map_err(|error| format!("opencode: {error}"))
}

fn build_adapter(config: &Config, api: Arc<dyn Api>) -> Arc<OpenCodeAdapter> {
    let model =
        config.adapter.model.as_ref().and_then(|model| {
            ModelRef::new(&model.provider_id, &model.id, model.variant.as_deref())
        });
    Arc::new(OpenCodeAdapter::new(
        api,
        config.adapter.agent.as_deref(),
        model,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn discover_once(
    api: &dyn Api,
    config: &Config,
    logger: &Logger,
    map: &mut ProjectMap,
    owners: &Arc<RwLock<TopicOwners>>,
    topics_tx: &watch::Sender<Vec<String>>,
    topics_count: &AtomicUsize,
    previous_keys: &mut Vec<String>,
    warning_signature: &mut String,
    detail_signature: &mut String,
) {
    let projects = match api.projects().await {
        Ok(projects) => projects,
        Err(error) => {
            logger.error(&format!("project discovery failed: {error}"));
            return;
        }
    };
    let next = build_project_map(&projects, &config.project_roots);
    if let Ok(mut owners) = owners.write() {
        owners.clone_from(next.owners());
    }

    let mut warnings: Vec<&str> = next.warnings().iter().map(String::as_str).collect();
    let mut sorted_warnings = warnings.clone();
    sorted_warnings.sort_unstable();
    let signature = sorted_warnings.join("\n");
    if signature != *warning_signature {
        for warning in warnings.drain(..) {
            logger.warn(warning);
        }
        *warning_signature = signature;
    }

    let detail = next
        .topics()
        .into_iter()
        .filter_map(|topic| {
            next.get(&topic).map(|entry| {
                let mut ids = entry.project_ids.clone();
                ids.sort_unstable();
                format!("{}|{}|{}", topic, entry.canonical.display(), ids.join("+"))
            })
        })
        .collect::<Vec<_>>()
        .join("\n");
    if detail != *detail_signature {
        *detail_signature = detail;
        let keys = next.topics();
        logger.info(&format!(
            "topics now {}: {}",
            next.len(),
            if keys.is_empty() {
                "(none)".to_string()
            } else {
                keys.join(",")
            }
        ));
    }

    let mut keys = next.topics();
    keys.sort_unstable();
    if keys != *previous_keys {
        *previous_keys = keys;
        let ordered = next.topics();
        let _ = topics_tx.send(ordered);
    }
    topics_count.store(next.len(), Ordering::Relaxed);
    *map = next;
}

async fn prune_now(config: &Config, logger: &Logger) {
    let roots = config.project_roots.clone();
    let inbox_dir = config.files.inbox_dir.clone();
    let totals = tokio::task::spawn_blocking(move || {
        prune_inboxes(&roots, &inbox_dir, now_ms(), INBOX_RETENTION_MS)
    })
    .await;
    let totals = match totals {
        Ok(totals) => totals,
        Err(error) => {
            logger.warn(&format!("inbox prune task failed: {error}"));
            return;
        }
    };
    for (root, error) in &totals.failed_roots {
        logger.warn(&format!(
            "inbox prune failed for {}: {}",
            root.display(),
            clip(error, 300)
        ));
    }
    logger.info(&format!(
        "inbox-prune roots={} removed={} bytes={} failed={}",
        totals.roots, totals.removed, totals.bytes, totals.failed
    ));
}

/// Releasing on drop covers every early return after the lock is taken.
struct LockGuard {
    path: PathBuf,
    pid: u32,
}

impl LockGuard {
    fn new(path: &std::path::Path, pid: u32) -> Self {
        Self {
            path: path.to_path_buf(),
            pid,
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        crate::lock::release_lock(&self.path, self.pid);
    }
}

#[cfg(unix)]
async fn wait_for_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    if let Ok(mut terminate) = signal(SignalKind::terminate()) {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = terminate.recv() => "SIGTERM",
        }
    } else {
        let _ = tokio::signal::ctrl_c().await;
        "SIGINT"
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "SIGINT"
}
