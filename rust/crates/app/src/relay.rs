//! Inbound message handling and push policy.
//!
//! Mirrors `Relay.handleMessage`, `handleControl`, `handleNativeCommand`,
//! `deliverPrompt`, `replyPermission`, `ack`, and `publishError` in
//! `ntfy-inbox-relay.mjs`. The SSE reader and the queue live in `sse.rs` and
//! `runner.rs`; this module owns what one accepted message does.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ntfy_relay_core::adapter::{
    Delivery, FormApi, PermissionApi, ProjectEntry, PromptFile, SessionApi, TargetSink,
};
use ntfy_relay_core::asks::AskStore;
use ntfy_relay_core::config::Config;
use ntfy_relay_core::forms::FormStore;
use ntfy_relay_core::link_safe::link_safe;
use ntfy_relay_core::text::{clamp_bytes, trim_js, ACK_MAX_BYTES};
use ntfy_relay_inbound::control::engine::{Control, Decision};
use ntfy_relay_inbound::control::parse::{parse_native_command, ParsedNative};
use ntfy_relay_inbound::filter::{should_accept, AcceptOptions};
use ntfy_relay_inbound::form_reply::{apply_form_reply, FormError};
use ntfy_relay_inbound::permission::{decision_for_native, reply_to_permission, PermissionError};
use ntfy_relay_inbound::prompting::{deliver_prompt, delivery_ack, DeliveryRequest};
use ntfy_relay_inbound::queue::{MessageQueue, QueuedMessage};
use ntfy_relay_inbound::rate::{PromptRate, PROMPTS_PER_MINUTE};
use ntfy_relay_inbound::routing::{Hints, Source, Windows};
use ntfy_relay_ntfy::attach::{fetch_attachment, Attachment};
use ntfy_relay_ntfy::header::Priority;
use ntfy_relay_ntfy::http::Transport;
use ntfy_relay_ntfy::publish::{HeaderText, NtfyServer, Publisher};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Notify;

use crate::discovery::ProjectMap;
use crate::logger::Logger;
use crate::store::StateStore;
use crate::util::{clip, now_ms, project_name};

/// Messages handled by one `drain` call before the loop gets to poll discovery,
/// prune, and the supervisor again.
pub const DRAIN_BATCH: usize = 8;
/// Per-project inbox byte budget. Over budget is one error push and no prompt.
pub const INBOX_BUDGET_BYTES: u64 = 200 * 1024 * 1024;

pub struct Relay {
    config: Config,
    logger: Logger,
    store: Arc<StateStore>,
    adapter: Arc<dyn SessionApi>,
    permissions: Arc<dyn PermissionApi>,
    form_api: Arc<dyn FormApi>,
    asks: Arc<AskStore>,
    forms: Arc<FormStore>,
    publisher: Publisher,
    transport: Arc<dyn Transport>,
    last_processed: Arc<AtomicI64>,
    server: NtfyServer,
    deploy_config: PathBuf,
    prompt_rate: Mutex<PromptRate>,
    inbox_budget: u64,
}

impl Relay {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        logger: Logger,
        store: Arc<StateStore>,
        adapter: Arc<dyn SessionApi>,
        permissions: Arc<dyn PermissionApi>,
        form_api: Arc<dyn FormApi>,
        asks: Arc<AskStore>,
        forms: Arc<FormStore>,
        publisher: Publisher,
        transport: Arc<dyn Transport>,
        last_processed: Arc<AtomicI64>,
        server: NtfyServer,
        deploy_config: PathBuf,
    ) -> Self {
        Self {
            config,
            logger,
            store,
            adapter,
            permissions,
            form_api,
            asks,
            forms,
            publisher,
            transport,
            last_processed,
            server,
            deploy_config,
            prompt_rate: Mutex::new(PromptRate::new()),
            inbox_budget: INBOX_BUDGET_BYTES,
        }
    }

    /// Test seam for the fixed 200 MB budget; production keeps the constant.
    #[cfg(test)]
    pub fn set_inbox_budget(&mut self, bytes: u64) {
        self.inbox_budget = bytes;
    }

    pub async fn handle_message(
        &self,
        queued: QueuedMessage,
        entry: &ProjectEntry,
        control: &mut Control<'_>,
    ) {
        let message = &queued.message;
        let (ids, last_time) = self.store.topic_cursor(&message.topic);
        let options = AcceptOptions {
            topic: &entry.topic,
            now_ms: queued.received_at,
            replay_window_minutes: self.config.ntfy.replay_window_minutes as i64,
            max_message_bytes: self.config.ntfy.max_message_bytes,
            processed_ids: &ids,
            last_time,
        };
        if let Err(reason) = should_accept(message, &options) {
            self.logger.debug(&format!(
                "filtered {} topic={} id={}",
                reason.as_str(),
                message.topic,
                dash_if_empty(&message.id)
            ));
            return;
        }

        let message_id = message.id.clone();
        let message_time = message.time.unwrap_or(0);
        if let Err(error) = self
            .store
            .record_processed(&message.topic, &message_id, message_time)
            .await
        {
            self.logger
                .error(&format!("message handler failed: state: {error}"));
            return;
        }
        self.last_processed
            .fetch_max(message_time, Ordering::Relaxed);
        self.logger.info(&format!(
            "accepted topic={} id={} bytes={}",
            message.topic,
            message_id,
            message.message.len()
        ));

        let text = trim_js(&message.message).to_string();
        let decision = control.resolve(&text, &message.topic);
        let native = if matches!(decision, Decision::Prompt) {
            parse_native_command(&text)
        } else {
            None
        };

        if !matches!(decision, Decision::Prompt) || native.is_some() {
            if message.attachment.is_some() {
                self.publish_error(entry, "commands cannot carry attachments")
                    .await;
                return;
            }
            match native {
                Some(native) => {
                    self.handle_native(entry, native, &message_id, queued.received_at)
                        .await
                }
                None => {
                    self.handle_control(decision, entry, &message_id, control)
                        .await
                }
            }
            return;
        }

        // Fixed prompt rate: 30 per minute per topic with a burst of 10. Commands
        // above are exempt; one token gates one agent run.
        {
            let allowed = self
                .prompt_rate
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .allow(&message.topic, queued.received_at);
            if !allowed {
                self.publish_error(
                    entry,
                    &format!(
                        "rate limited: at most {PROMPTS_PER_MINUTE} prompts per minute per topic"
                    ),
                )
                .await;
                return;
            }
        }

        let mut files = Vec::new();
        if let Some(attachment) = &message.attachment {
            let inbox = entry.canonical.join(&self.config.files.inbox_dir);
            let declared = attachment.size.and_then(|size| u64::try_from(size).ok());
            let need = declared.unwrap_or(0);
            let used = match make_room_blocking(inbox.clone(), need, self.inbox_budget).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.logger
                        .warn(&format!("inbox budget check failed: {error}"));
                    0
                }
            };
            if used.saturating_add(need) > self.inbox_budget {
                self.publish_error(
                    entry,
                    &format!(
                        "inbox budget exceeded: {} of {} MB used; message not prompted",
                        used / (1024 * 1024),
                        self.inbox_budget / (1024 * 1024)
                    ),
                )
                .await;
                return;
            }
            let attachment = Attachment {
                url: attachment.url.clone(),
                name: if attachment.name.is_empty() {
                    None
                } else {
                    Some(attachment.name.clone())
                },
                size: attachment.size.and_then(|size| u64::try_from(size).ok()),
            };
            let deploy_text = std::fs::read_to_string(&self.deploy_config).ok();
            let allowed_hosts = attachment_hosts(self.server.host(), deploy_text.as_deref());
            match fetch_attachment(
                self.transport.as_ref(),
                &attachment,
                &allowed_hosts,
                self.server.as_str(),
                &inbox,
                self.config.files.max_file_bytes,
                now_ms(),
            )
            .await
            {
                Ok(saved) => {
                    self.logger.info(&format!(
                        "attachment saved {} {} bytes",
                        saved.path.display(),
                        saved.bytes
                    ));
                    files.push(PromptFile {
                        path: saved.path,
                        name: saved.name,
                    });
                    // Trim an overshoot: the new file is the newest, so the
                    // oldest inbox files go first.
                    if let Err(error) =
                        make_room_blocking(inbox.clone(), 0, self.inbox_budget).await
                    {
                        self.logger
                            .warn(&format!("inbox budget trim failed: {error}"));
                    }
                }
                Err(error) => {
                    self.logger
                        .error(&format!("attachment failed id={message_id}: {error}"));
                    self.publish_error(
                        entry,
                        &format!("attachment failed: {}", clip(&error.to_string(), 200)),
                    )
                    .await;
                    return;
                }
            }
        }

        self.deliver(
            entry,
            &text,
            self.config.routing.delivery,
            &files,
            &message_id,
        )
        .await;
    }

    /// Drains the queue in arrival order, one message at a time, at most
    /// `DRAIN_BATCH` messages per call. The queue is released between messages
    /// so the SSE reader can keep filling it, and a non-empty queue re-arms the
    /// loop so discovery, prune, and the supervisor still get polled.
    pub async fn drain(
        &self,
        queue: &AsyncMutex<MessageQueue>,
        queue_len: &AtomicUsize,
        projects: &ProjectMap,
        control: &mut Control<'_>,
        stopping: &AtomicBool,
        notify: &Notify,
    ) {
        let mut handled = 0usize;
        loop {
            if stopping.load(Ordering::Relaxed) {
                break;
            }
            if handled >= DRAIN_BATCH {
                let pending = { queue.lock().await.len() };
                if pending > 0 {
                    notify.notify_one();
                }
                break;
            }
            let next = {
                let mut queue = queue.lock().await;
                let item = queue.pop();
                queue_len.store(queue.len(), Ordering::Relaxed);
                item
            };
            let Some(queued) = next else {
                break;
            };
            let Some(entry) = projects.get(&queued.message.topic).cloned() else {
                self.logger.debug(&format!(
                    "filtered unknown-topic topic={} id={}",
                    queued.message.topic,
                    dash_if_empty(&queued.message.id)
                ));
                handled += 1;
                continue;
            };
            self.handle_message(queued, &entry, control).await;
            handled += 1;
        }
    }

    /// Waits for queued state writes, used once on shutdown.
    pub async fn flush(&self) {
        self.store.flush().await;
    }

    async fn handle_native(
        &self,
        entry: &ProjectEntry,
        native: ParsedNative,
        message_id: &str,
        now: i64,
    ) {
        match native.verb {
            "steer" => {
                if native.arg.is_empty() {
                    self.ack(entry, "steer", "usage: /steer <text>", Priority::Min)
                        .await;
                } else if !self
                    .prompt_rate
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .allow(&entry.topic, now)
                {
                    self.ack(
                        entry,
                        "error",
                        &format!(
                            "rate limited: at most {PROMPTS_PER_MINUTE} prompts per minute per topic"
                        ),
                        Priority::High,
                    )
                    .await;
                } else {
                    self.deliver(entry, &native.arg, Delivery::Steer, &[], message_id)
                        .await;
                }
            }
            "answer" => {
                match apply_form_reply(
                    self.forms.as_ref(),
                    self.form_api.as_ref(),
                    &entry.topic,
                    &native.arg,
                )
                .await
                {
                    Ok(outcome) => {
                        self.logger.info(&format!(
                            "form cancelled title={} bytes={}",
                            outcome.title,
                            native.arg.len()
                        ));
                        self.ack(entry, "question", &outcome.ack_body(), Priority::Min)
                            .await
                    }
                    Err(error @ (FormError::NoForm | FormError::AlreadyAnswered { .. })) => {
                        self.ack(entry, "error", &error.message(), Priority::High)
                            .await
                    }
                    Err(error) => {
                        self.logger.error(&format!(
                            "answer failed id={message_id}: {}",
                            clip(&error.message(), 500)
                        ));
                        let status = error
                            .status()
                            .map(|status| format!("HTTP {status}: "))
                            .unwrap_or_default();
                        self.publish_error(
                            entry,
                            &format!("answer: {status}{}", clip(&error.message(), 200)),
                        )
                        .await;
                    }
                }
            }
            verb => {
                let Some(decision) = decision_for_native(verb) else {
                    self.ack(
                        entry,
                        "error",
                        &format!("unknown command {verb}"),
                        Priority::High,
                    )
                    .await;
                    return;
                };
                match reply_to_permission(
                    self.permissions.as_ref(),
                    self.asks.as_ref(),
                    decision,
                    native.arg_token(),
                    &entry.topic,
                )
                .await
                {
                    Ok(outcome) => {
                        self.logger.info(&format!(
                            "permission reply code={} decision={}",
                            outcome.code(),
                            decision.as_str()
                        ));
                        self.ack(entry, "permission", &outcome.ack_body(), Priority::Min)
                            .await
                    }
                    Err(error @ (PermissionError::NoAsk | PermissionError::UnknownCode { .. })) => {
                        self.ack(entry, "error", &error.message(), Priority::High)
                            .await
                    }
                    Err(error) => {
                        self.logger.error(&format!(
                            "{verb} failed id={message_id}: {}",
                            clip(&error.message(), 500)
                        ));
                        let status = error
                            .status()
                            .map(|status| format!("HTTP {status}: "))
                            .unwrap_or_default();
                        self.publish_error(
                            entry,
                            &format!("{verb}: {status}{}", clip(&error.message(), 200)),
                        )
                        .await;
                    }
                }
            }
        }
    }

    async fn handle_control(
        &self,
        decision: Decision,
        entry: &ProjectEntry,
        message_id: &str,
        control: &mut Control<'_>,
    ) {
        match decision {
            Decision::Unknown { .. } => {
                self.ack(
                    entry,
                    "control",
                    "unknown action; send /help for the list",
                    Priority::High,
                )
                .await
            }
            Decision::Disabled { verb, missing } => {
                let names: Vec<&str> = missing
                    .iter()
                    .map(|capability| capability.as_str())
                    .collect();
                self.ack(
                    entry,
                    "error",
                    &format!("{verb}: missing capability {}", names.join(", ")),
                    Priority::High,
                )
                .await
            }
            Decision::Busy { .. } => {
                self.ack(
                    entry,
                    "confirm",
                    "confirmation pending; answer /yes or /no",
                    Priority::High,
                )
                .await
            }
            Decision::Ask { text, .. } => self.ack(entry, "confirm", &text, Priority::Min).await,
            Decision::Expired => {
                self.ack(entry, "confirm", "confirmation expired", Priority::Min)
                    .await
            }
            Decision::Cancelled => self.ack(entry, "confirm", "cancelled", Priority::Min).await,
            Decision::None => {
                self.ack(entry, "confirm", "nothing to confirm", Priority::Min)
                    .await
            }
            Decision::Run(run) => {
                let verb = run.verb.clone();
                match control.execute(run, entry).await {
                    Ok(text) => {
                        self.ack(entry, &verb, &text, Priority::Min).await;
                        self.logger
                            .info(&format!("control {verb} ok id={message_id}"));
                    }
                    Err(error) => {
                        self.logger.error(&format!(
                            "control {verb} failed id={message_id}: {}",
                            clip(&error.to_string(), 500)
                        ));
                        self.publish_error(
                            entry,
                            &format!("{verb}: {}", clip(&error.to_string(), 200)),
                        )
                        .await;
                    }
                }
            }
            Decision::Prompt => {}
        }
    }

    async fn deliver(
        &self,
        entry: &ProjectEntry,
        text: &str,
        delivery: Delivery,
        files: &[PromptFile],
        message_id: &str,
    ) {
        let target = self.store.target(&entry.topic);
        let hints = Hints {
            last: target.last.as_deref(),
            last_at: target.last_at,
            pending: target.pending.as_deref(),
            pending_at: target.pending_at,
        };
        match deliver_prompt(
            self.adapter.as_ref(),
            DeliveryRequest {
                entry,
                hints,
                prefer_running: self.config.routing.prefer_running,
                windows: Windows::default(),
                now: now_ms(),
                text,
                delivery,
                files,
            },
        )
        .await
        {
            Ok(resolved) => {
                let now = now_ms();
                self.store.set_last(&entry.topic, &resolved.session.id, now);
                if resolved.source == Source::Pending {
                    self.store.set_pending(&entry.topic, None, now);
                }
                let (suffix, body) = delivery_ack(&resolved.session, delivery);
                self.ack(entry, suffix, &body, Priority::Min).await;
                self.logger.info(&format!(
                    "prompted session={} source={} delivery={} files={} id={message_id}",
                    resolved.session.id,
                    resolved.source.as_str(),
                    delivery.as_str(),
                    files.len()
                ));
            }
            Err(error) => {
                self.logger
                    .error(&format!("prompt failed id={message_id}: {error}"));
                let status = error
                    .status()
                    .map(|status| format!("HTTP {status}: "))
                    .unwrap_or_default();
                self.publish_error(entry, &format!("{status}{}", clip(&error.message(), 200)))
                    .await;
            }
        }
    }

    async fn ack(&self, entry: &ProjectEntry, suffix: &str, body: &str, priority: Priority) {
        let title = link_safe(&format!("{} - {suffix}", project_name(&entry.canonical)));
        let safe = link_safe(body);
        let message = clamp_bytes(&safe, ACK_MAX_BYTES);
        let text = HeaderText {
            topic: &entry.topic,
            title: &title,
            message: &message,
            priority,
            tags: &["oc-out"],
        };
        if let Err(error) = self.publisher.publish_header(&text).await {
            self.logger.error(&format!("ack failed: {error}"));
        }
    }

    async fn publish_error(&self, entry: &ProjectEntry, message: &str) {
        let title = link_safe(&format!("{} - relay error", project_name(&entry.canonical)));
        let body = link_safe(&clip(message, 200));
        let text = HeaderText {
            topic: &entry.topic,
            title: &title,
            message: &body,
            priority: Priority::High,
            tags: &["oc-out"],
        };
        if let Err(error) = self.publisher.publish_header(&text).await {
            self.logger.error(&format!("error push failed: {error}"));
        }
    }
}

fn dash_if_empty(value: &str) -> &str {
    if value.is_empty() {
        "-"
    } else {
        value
    }
}

/// Allowed attachment hosts: the ntfy server host plus the deployment baseUrl
/// host when `ntfy-deploy.json` parses. The file is read per attachment so a
/// mode switch takes effect without a relay restart; a missing or invalid file
/// leaves the server host only.
fn attachment_hosts(server_host: &str, deploy_text: Option<&str>) -> Vec<String> {
    let mut hosts = vec![server_host.to_string()];
    let Some(text) = deploy_text else {
        return hosts;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return hosts;
    };
    let Some(base) = value.get("baseUrl").and_then(|value| value.as_str()) else {
        return hosts;
    };
    let Some(host) = url_host(base) else {
        return hosts;
    };
    if !hosts.iter().any(|known| known.eq_ignore_ascii_case(&host)) {
        hosts.push(host);
    }
    hosts
}

fn url_host(value: &str) -> Option<String> {
    let rest = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next()?
    } else {
        authority.split(':').next()?
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

async fn make_room_blocking(
    dir: std::path::PathBuf,
    need: u64,
    budget: u64,
) -> std::io::Result<u64> {
    tokio::task::spawn_blocking(move || ntfy_relay_inbound::prune::make_room(&dir, need, budget))
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use ntfy_relay_core::adapter::{
        AdapterError, Capability, NewSession, Permission, PermissionDecision, PromptRequest,
        Session,
    };
    use ntfy_relay_core::clock::Clock;
    use ntfy_relay_core::config::{load_config_from_text, LogLevel};
    use ntfy_relay_core::state::State;
    use ntfy_relay_core::test_support::Scratch;
    use ntfy_relay_inbound::control::table::builtin_control_table;
    use ntfy_relay_inbound::filter::{Attachment, InboundMessage};
    use ntfy_relay_inbound::queue::MessageQueue;
    use ntfy_relay_ntfy::http::{HttpError, HttpRequest, HttpResponse};
    use ntfy_relay_ntfy::publish::NtfyServer;
    use ntfy_relay_opencode::api::{ProjectInfo, ProjectTime};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::{Arc, Mutex};

    use crate::discovery::build_project_map;

    #[test]
    fn attachment_hosts_include_the_deployment_base_url_host() {
        let deploy = r#"{ "version": 1, "mode": "vpn", "baseUrl": "http://100.64.0.10:8810" }"#;
        let hosts = attachment_hosts("127.0.0.1", Some(deploy));
        assert_eq!(hosts, vec!["127.0.0.1", "100.64.0.10"]);
    }

    #[test]
    fn attachment_hosts_ignore_a_missing_or_invalid_file() {
        assert_eq!(attachment_hosts("127.0.0.1", None), vec!["127.0.0.1"]);
        assert_eq!(
            attachment_hosts("127.0.0.1", Some("not json")),
            vec!["127.0.0.1"]
        );
        assert_eq!(
            attachment_hosts("127.0.0.1", Some(r#"{ "mode": "proxy" }"#)),
            vec!["127.0.0.1"]
        );
    }

    #[test]
    fn attachment_hosts_deduplicate_and_strip_the_port() {
        let deploy = r#"{ "baseUrl": "https://ntfy.example.ts.net" }"#;
        assert_eq!(
            attachment_hosts("ntfy.example.ts.net", Some(deploy)),
            vec!["ntfy.example.ts.net"]
        );
        let bracketed = r#"{ "baseUrl": "http://[::1]:8810" }"#;
        assert_eq!(
            attachment_hosts("127.0.0.1", Some(bracketed)),
            vec!["127.0.0.1", "::1"]
        );
    }

    const CAPABILITIES: [Capability; 5] = [
        Capability::SessionCreate,
        Capability::SessionList,
        Capability::SessionPin,
        Capability::SessionAuto,
        Capability::Prompt,
    ];

    struct PromptRecord {
        session: String,
        text: String,
        delivery: Delivery,
        files: Vec<PromptFile>,
    }

    #[derive(Default)]
    struct FakeAdapter {
        sessions: Mutex<Vec<Session>>,
        created: Mutex<Option<String>>,
        prompts: Mutex<Vec<PromptRecord>>,
        fail_prompt: AtomicBool,
        replies: Mutex<Vec<(String, String, PermissionDecision)>>,
        forms: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl SessionApi for FakeAdapter {
        fn capabilities(&self) -> &[Capability] {
            &CAPABILITIES
        }

        async fn create_session(&self, request: NewSession<'_>) -> Result<Session, AdapterError> {
            *self.created.lock().unwrap() = Some(request.title.unwrap_or("").to_string());
            Ok(Session {
                id: "ses_new".to_string(),
                title: request.title.map(str::to_string),
                running: false,
                updated: 1,
                project_id: request.entry.project_ids.first().cloned(),
            })
        }

        async fn list_sessions(
            &self,
            _entry: &ProjectEntry,
            limit: usize,
        ) -> Result<Vec<Session>, AdapterError> {
            Ok(self
                .sessions
                .lock()
                .unwrap()
                .iter()
                .take(limit)
                .cloned()
                .collect())
        }

        async fn prompt(&self, request: PromptRequest<'_>) -> Result<(), AdapterError> {
            self.prompts.lock().unwrap().push(PromptRecord {
                session: request.session_id.to_string(),
                text: request.text.to_string(),
                delivery: request.delivery,
                files: request.files.to_vec(),
            });
            if self.fail_prompt.load(Ordering::Relaxed) {
                return Err(AdapterError::with_status(500, "boom"));
            }
            Ok(())
        }
    }

    #[async_trait]
    impl PermissionApi for FakeAdapter {
        async fn list_permissions(
            &self,
            _session_id: &str,
        ) -> Result<Vec<Permission>, AdapterError> {
            Ok(Vec::new())
        }

        async fn reply_permission(
            &self,
            session_id: &str,
            permission_id: &str,
            decision: PermissionDecision,
        ) -> Result<(), AdapterError> {
            self.replies.lock().unwrap().push((
                session_id.to_string(),
                permission_id.to_string(),
                decision,
            ));
            Ok(())
        }
    }

    #[async_trait]
    impl FormApi for FakeAdapter {
        async fn cancel_form(
            &self,
            session_id: &str,
            form_id: &str,
            message: &str,
        ) -> Result<(), AdapterError> {
            self.forms.lock().unwrap().push((
                session_id.to_string(),
                form_id.to_string(),
                message.to_string(),
            ));
            Ok(())
        }
    }

    #[derive(Default)]
    struct Recorder {
        requests: Mutex<Vec<HttpRequest>>,
    }

    #[async_trait]
    impl Transport for Recorder {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
            self.requests.lock().unwrap().push(request);
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: Bytes::new(),
            })
        }
    }

    struct FixedClock;

    impl Clock for FixedClock {
        fn now_ms(&self) -> i64 {
            1_700_000_000_000
        }
    }

    struct Harness {
        relay: Relay,
        adapter: Arc<FakeAdapter>,
        transport: Arc<Recorder>,
        store: Arc<StateStore>,
        asks: Arc<AskStore>,
        forms: Arc<FormStore>,
        root: PathBuf,
        /// Keeps the scratch directory alive for the whole test.
        _dir: Scratch,
    }

    fn temp_root(name: &str) -> Scratch {
        Scratch::new(&format!("app-relay-{name}"))
    }

    fn harness(name: &str) -> Harness {
        harness_with_state(name, State::new())
    }

    fn harness_with_state(name: &str, state: State) -> Harness {
        let root = temp_root(name);
        let config = load_config_from_text(
            &json!({
                "ntfy": { "server": "https://ntfy.test", "tokenFile": "token.txt" },
                "projectRoots": [root.to_string_lossy()],
                "adapter": {
                    "name": "opencode",
                    "agent": "build",
                    "model": { "providerID": "p", "id": "m" }
                },
                "logging": { "file": root.join("relay.log").to_string_lossy() },
                "stateFile": root.join("state.json").to_string_lossy()
            })
            .to_string(),
            Some(&root.join("ntfy-relay.json")),
            &root,
        )
        .unwrap();
        let logger = Logger::new(None, LogLevel::Debug, false, 1024);
        let store = Arc::new(StateStore::new(
            root.join("state.json"),
            state,
            logger.clone(),
        ));
        let adapter = Arc::new(FakeAdapter::default());
        adapter
            .sessions
            .lock()
            .unwrap()
            .push(session("ses_1", "Fix links"));
        let transport = Arc::new(Recorder::default());
        let server = NtfyServer::parse("https://ntfy.test").unwrap();
        let publisher = Publisher::new(
            server.clone(),
            Some("token-1234567890".to_string()),
            transport.clone(),
        );
        let (asks, _) = AskStore::open(
            root.join(ntfy_relay_core::asks::ASK_FILE),
            1_700_000_000_000,
        );
        let asks = Arc::new(asks);
        let (forms, _) = ntfy_relay_core::forms::FormStore::open(
            root.join(ntfy_relay_core::forms::FORM_FILE),
            1_700_000_000_000,
        );
        let forms = Arc::new(forms);
        let relay = Relay::new(
            config,
            logger,
            store.clone(),
            adapter.clone(),
            adapter.clone(),
            adapter.clone(),
            asks.clone(),
            forms.clone(),
            publisher,
            transport.clone(),
            Arc::new(AtomicI64::new(0)),
            server,
            root.join("ntfy-deploy.json"),
        );
        Harness {
            relay,
            adapter,
            transport,
            store,
            asks,
            forms,
            root: root.path().to_path_buf(),
            _dir: root,
        }
    }

    fn session(id: &str, title: &str) -> Session {
        Session {
            id: id.to_string(),
            title: Some(title.to_string()),
            running: false,
            updated: 100,
            project_id: Some("p1".to_string()),
        }
    }

    fn entry(harness: &Harness) -> ProjectEntry {
        ProjectEntry {
            topic: "proj".to_string(),
            canonical: harness.root.join("proj"),
            project_ids: vec!["p1".to_string()],
        }
    }

    fn message(text: &str) -> InboundMessage {
        InboundMessage {
            id: "m1".to_string(),
            event: "message".to_string(),
            topic: "proj".to_string(),
            time: Some(1_700_000_000),
            message: text.to_string(),
            ..InboundMessage::default()
        }
    }

    fn queued(text: &str) -> QueuedMessage {
        QueuedMessage {
            message: message(text),
            received_at: 1_700_000_000_000,
        }
    }

    fn bodies(transport: &Recorder) -> Vec<(String, String, String)> {
        transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| {
                (
                    request.url.clone(),
                    request
                        .header_value("Title")
                        .unwrap_or_default()
                        .to_string(),
                    String::from_utf8_lossy(&request.body).to_string(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn a_prompt_is_delivered_and_acked() {
        let harness = harness("prompt");
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        harness
            .relay
            .handle_message(
                queued("queued fix readme.md links"),
                &entry(&harness),
                &mut control,
            )
            .await;

        let prompts = harness.adapter.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].session, "ses_1");
        assert_eq!(prompts[0].text, "queued fix readme.md links");
        assert_eq!(prompts[0].delivery, Delivery::Queue);
        assert!(prompts[0].files.is_empty());

        let published = bodies(&harness.transport);
        assert_eq!(published.len(), 1);
        assert!(published[0].0.ends_with("/proj"));
        assert_eq!(published[0].1, "proj - queued");
        assert_eq!(published[0].2, "queued to Fix links");
        assert_eq!(
            harness.transport.requests.lock().unwrap()[0].header_value("Tags"),
            Some("oc-out")
        );

        let (ids, _) = harness.store.topic_cursor("proj");
        assert_eq!(ids, vec!["m1".to_string()]);
    }

    #[tokio::test]
    async fn ack_text_is_link_safe() {
        let harness = harness("ack");
        harness
            .relay
            .ack(&entry(&harness), "note", "readme.md", Priority::Min)
            .await;
        let published = bodies(&harness.transport);
        assert_eq!(published[0].2, "readme[.]md");
    }

    #[tokio::test]
    async fn a_failed_prompt_still_leaves_the_message_recorded() {
        let harness = harness("failed");
        harness.adapter.fail_prompt.store(true, Ordering::Relaxed);
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        harness
            .relay
            .handle_message(queued("do the thing"), &entry(&harness), &mut control)
            .await;

        let (ids, _) = harness.store.topic_cursor("proj");
        assert_eq!(ids, vec!["m1".to_string()], "state precedes delivery");
        let published = bodies(&harness.transport);
        assert_eq!(published[0].1, "proj - relay error");
        assert!(published[0].2.contains("HTTP 500: boom"), "{:?}", published);
    }

    #[tokio::test]
    async fn an_attachment_failure_skips_the_prompt_and_pushes_an_error() {
        let harness = harness("attachment");
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        let mut queued = queued("see file");
        queued.message.attachment = Some(Attachment {
            url: "http://ntfy.test/file".to_string(),
            name: "file.png".to_string(),
            size: None,
        });
        harness
            .relay
            .handle_message(queued, &entry(&harness), &mut control)
            .await;

        assert!(harness.adapter.prompts.lock().unwrap().is_empty());
        let (ids, _) = harness.store.topic_cursor("proj");
        assert_eq!(ids, vec!["m1".to_string()]);
        let published = bodies(&harness.transport);
        assert!(published[0]
            .2
            .contains("attachment failed: attachment: HTTPS required"));
    }

    #[tokio::test]
    async fn help_answers_without_prompting() {
        let harness = harness("help");
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        harness
            .relay
            .handle_message(queued("/help"), &entry(&harness), &mut control)
            .await;

        assert!(harness.adapter.prompts.lock().unwrap().is_empty());
        let published = bodies(&harness.transport);
        assert!(published[0].2.contains("/new"), "{:?}", published);
    }

    #[tokio::test]
    async fn answer_sends_the_free_text_to_the_newest_form() {
        let harness = harness("form-answer");
        harness
            .forms
            .register(ntfy_relay_core::forms::FormRecord {
                code: String::new(),
                form: "frm_1".to_string(),
                session: "ses_form".to_string(),
                topic: "proj".to_string(),
                title: "Release checks".to_string(),
                pushed: 1_700_000_000_000,
                fields: Vec::new(),
            })
            .unwrap();
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        harness
            .relay
            .handle_message(
                queued("/answer 1 keep 2 any"),
                &entry(&harness),
                &mut control,
            )
            .await;

        assert_eq!(
            *harness.adapter.forms.lock().unwrap(),
            vec![(
                "ses_form".to_string(),
                "frm_1".to_string(),
                "1 keep 2 any".to_string()
            )]
        );
        assert_eq!(
            bodies(&harness.transport)[0].2,
            "answer sent for Release checks"
        );
        assert_eq!(harness.forms.count(), 0);
        assert!(harness.adapter.prompts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn answer_without_a_form_names_no_session() {
        let harness = harness("form-empty");
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        harness
            .relay
            .handle_message(queued("/answer yes please"), &entry(&harness), &mut control)
            .await;

        assert_eq!(bodies(&harness.transport)[0].2, "no pending question form");
        assert!(harness.adapter.forms.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn allow_replies_to_the_registered_ask_not_the_target() {
        let mut state = State::new();
        state.set_last("proj", "ses_1", 0);
        let harness = harness_with_state("permission", state);
        let registered = harness
            .asks
            .register(
                "per_2",
                "ses_other",
                "proj",
                "Other work",
                1_700_000_000_000,
            )
            .unwrap();
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        harness
            .relay
            .handle_message(queued("/allow"), &entry(&harness), &mut control)
            .await;

        assert_eq!(
            *harness.adapter.replies.lock().unwrap(),
            vec![(
                "ses_other".to_string(),
                "per_2".to_string(),
                PermissionDecision::Once
            )]
        );
        assert_eq!(
            bodies(&harness.transport)[0].2,
            format!("approved once in Other work ({})", registered.code)
        );
        assert_eq!(harness.asks.count(), 0);
    }

    #[tokio::test]
    async fn a_named_code_resolves_the_older_ask() {
        let harness = harness("permission-code");
        let older = harness
            .asks
            .register("per_1", "ses_a", "proj", "Older", 1_700_000_000_000)
            .unwrap();
        harness
            .asks
            .register("per_2", "ses_b", "proj", "Newer", 1_700_000_000_100)
            .unwrap();
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        harness
            .relay
            .handle_message(
                queued(&format!("/ALLOW #{}", older.code.to_uppercase())),
                &entry(&harness),
                &mut control,
            )
            .await;

        assert_eq!(
            *harness.adapter.replies.lock().unwrap(),
            vec![(
                "ses_a".to_string(),
                "per_1".to_string(),
                PermissionDecision::Once
            )]
        );
        assert_eq!(
            bodies(&harness.transport)[0].2,
            format!("approved once in Older ({}), 1 more pending", older.code)
        );
        assert_eq!(harness.asks.count(), 1);
    }

    #[tokio::test]
    async fn allow_without_asks_names_no_session() {
        let harness = harness("permission-empty");
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        harness
            .relay
            .handle_message(queued("/allow"), &entry(&harness), &mut control)
            .await;
        assert!(harness.adapter.replies.lock().unwrap().is_empty());
        assert_eq!(bodies(&harness.transport)[0].2, "no pending permission ask");
    }

    #[tokio::test]
    async fn status_reports_the_ask_count() {
        let harness = harness("status-asks");
        harness
            .asks
            .register("per_1", "ses_1", "proj", "Fix links", 1_700_000_000_000)
            .unwrap();
        let asks = harness.asks.clone();
        let status = move || ntfy_relay_inbound::control::engine::Status {
            asks: asks.count(),
            ..Default::default()
        };
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock)
            .with_status(&status);
        harness
            .relay
            .handle_message(queued("/status"), &entry(&harness), &mut control)
            .await;
        assert_eq!(
            bodies(&harness.transport)[0].2,
            "topics 0\nqueue 0\ntarget none\nasks 1"
        );
    }

    #[tokio::test]
    async fn new_creates_and_targets_a_session() {
        let harness = harness("new");
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock)
            .with_targets(harness.relay.store.as_ref());
        harness
            .relay
            .handle_message(queued("/new prune-check"), &entry(&harness), &mut control)
            .await;

        assert_eq!(
            harness.adapter.created.lock().unwrap().as_deref(),
            Some("prune-check")
        );
        assert_eq!(
            harness.relay.store.target("proj").pending.as_deref(),
            Some("ses_new")
        );
        assert!(bodies(&harness.transport)[0]
            .2
            .contains("new session ses_new"));
    }

    #[tokio::test]
    async fn to_takes_a_title_fragment_without_an_id() {
        let harness = harness("to");
        harness
            .adapter
            .sessions
            .lock()
            .unwrap()
            .push(session("ses_2", "Other work"));
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock)
            .with_targets(harness.relay.store.as_ref());
        harness
            .relay
            .handle_message(queued("/to Other"), &entry(&harness), &mut control)
            .await;

        assert_eq!(
            harness.relay.store.target("proj").pending.as_deref(),
            Some("ses_2")
        );
        assert_eq!(
            bodies(&harness.transport)[0].2,
            "next message -> Other work"
        );
    }

    fn message_with_id(id: &str, text: &str) -> InboundMessage {
        let mut message = message(text);
        message.id = id.to_string();
        message
    }

    #[tokio::test]
    async fn drain_handles_messages_serially_in_arrival_order() {
        let harness = harness("drain");
        let projects = build_project_map(
            &[ProjectInfo {
                id: "p1".to_string(),
                canonical: Some(harness.root.join("proj").to_string_lossy().into_owned()),
                time: ProjectTime { active: 1 },
            }],
            std::slice::from_ref(&harness.root),
        );
        let queue = AsyncMutex::new(MessageQueue::new());
        {
            let mut queue = queue.lock().await;
            queue.push(message_with_id("m1", "first"), 1_700_000_000_000);
            queue.push(message_with_id("m2", "second"), 1_700_000_000_001);
        }
        let queue_len = AtomicUsize::new(2);
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        harness
            .relay
            .drain(
                &queue,
                &queue_len,
                &projects,
                &mut control,
                &AtomicBool::new(false),
                &Notify::new(),
            )
            .await;

        {
            let prompts = harness.adapter.prompts.lock().unwrap();
            assert_eq!(
                prompts
                    .iter()
                    .map(|prompt| prompt.text.as_str())
                    .collect::<Vec<_>>(),
                vec!["first", "second"]
            );
        }
        assert_eq!(queue_len.load(Ordering::Relaxed), 0);
        assert!(queue.lock().await.is_empty());
    }

    #[tokio::test]
    async fn drain_leaves_queued_messages_when_shutdown_wins() {
        let harness = harness("drain-stop");
        let projects = build_project_map(
            &[ProjectInfo {
                id: "p1".to_string(),
                canonical: Some(harness.root.join("proj").to_string_lossy().into_owned()),
                time: ProjectTime { active: 1 },
            }],
            std::slice::from_ref(&harness.root),
        );
        let queue = AsyncMutex::new(MessageQueue::new());
        {
            let mut queue = queue.lock().await;
            queue.push(message_with_id("m1", "first"), 1_700_000_000_000);
            queue.push(message_with_id("m2", "second"), 1_700_000_000_001);
        }
        let queue_len = AtomicUsize::new(2);
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        harness
            .relay
            .drain(
                &queue,
                &queue_len,
                &projects,
                &mut control,
                &AtomicBool::new(true),
                &Notify::new(),
            )
            .await;

        assert!(harness.adapter.prompts.lock().unwrap().is_empty());
        assert_eq!(queue.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn no_session_resolution_pushes_an_error() {
        let harness = harness("no-session");
        harness.adapter.sessions.lock().unwrap().clear();
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        harness
            .relay
            .handle_message(queued("hi"), &entry(&harness), &mut control)
            .await;

        assert!(harness.adapter.prompts.lock().unwrap().is_empty());
        let published = bodies(&harness.transport);
        assert_eq!(published[0].1, "proj - relay error");
        assert!(published[0]
            .2
            .contains("no session in project; relay never creates one"));
    }

    #[tokio::test]
    async fn an_ack_is_clamped_after_the_link_rewrite() {
        let plain = harness("clamp");
        plain
            .relay
            .ack(&entry(&plain), "note", &"x".repeat(5_000), Priority::Min)
            .await;
        let published = bodies(&plain.transport);
        assert_eq!(published[0].2.len(), ACK_MAX_BYTES);
        assert!(published[0].2.ends_with("..."));

        let dotted = harness("clamp-dots");
        let body = "a.md ".repeat(1_500);
        dotted
            .relay
            .ack(&entry(&dotted), "note", &body, Priority::Min)
            .await;
        let published = bodies(&dotted.transport);
        let message = &published[0].2;
        assert!(message.len() <= ACK_MAX_BYTES, "{}", message.len());
        assert!(message.len() <= 4096);
        assert!(!message.contains("a.md"), "a live dot survived");
        assert!(message.contains("a[.]md"));
    }

    #[tokio::test]
    async fn drain_stops_after_eight_messages_and_rearms() {
        let harness = harness("drain-batch");
        let projects = build_project_map(
            &[ProjectInfo {
                id: "p1".to_string(),
                canonical: Some(harness.root.join("proj").to_string_lossy().into_owned()),
                time: ProjectTime { active: 1 },
            }],
            std::slice::from_ref(&harness.root),
        );
        let queue = AsyncMutex::new(MessageQueue::new());
        {
            let mut queue = queue.lock().await;
            for index in 0..10 {
                queue.push(
                    message_with_id(&format!("m{index}"), &format!("text {index}")),
                    1_700_000_000_000 + index as i64,
                );
            }
        }
        let queue_len = AtomicUsize::new(10);
        let notify = Notify::new();
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        harness
            .relay
            .drain(
                &queue,
                &queue_len,
                &projects,
                &mut control,
                &AtomicBool::new(false),
                &notify,
            )
            .await;
        assert_eq!(harness.adapter.prompts.lock().unwrap().len(), DRAIN_BATCH);
        assert_eq!(queue.lock().await.len(), 2);
        assert_eq!(queue_len.load(Ordering::Relaxed), 2);

        harness
            .relay
            .drain(
                &queue,
                &queue_len,
                &projects,
                &mut control,
                &AtomicBool::new(false),
                &notify,
            )
            .await;
        assert_eq!(harness.adapter.prompts.lock().unwrap().len(), 10);
        assert!(queue.lock().await.is_empty());
    }

    #[tokio::test]
    async fn the_prompt_rate_is_thirty_per_minute_with_a_burst_of_ten() {
        let harness = harness("rate");
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        for index in 0..11 {
            harness
                .relay
                .handle_message(
                    QueuedMessage {
                        message: message_with_id(&format!("m{index}"), "prompt"),
                        received_at: 1_700_000_000_000,
                    },
                    &entry(&harness),
                    &mut control,
                )
                .await;
        }
        assert_eq!(harness.adapter.prompts.lock().unwrap().len(), 10);
        let published = bodies(&harness.transport);
        assert!(
            published
                .iter()
                .any(|(_, title, body)| title.contains("relay error")
                    && body.contains("rate limited")),
            "{published:?}"
        );
    }

    #[tokio::test]
    async fn an_inbox_over_budget_pushes_one_error_and_skips_the_prompt() {
        let mut harness = harness("budget");
        let inbox = harness.root.join("proj").join(".ntfy-inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("1700000000-old.pdf"), vec![0u8; 32]).unwrap();
        harness.relay.set_inbox_budget(16);
        let clock = FixedClock;
        let mut control = Control::new(builtin_control_table(), harness.adapter.as_ref(), &clock);
        let mut queued = queued("see file");
        queued.message.attachment = Some(Attachment {
            url: "https://ntfy.test/file".to_string(),
            name: "file.png".to_string(),
            size: Some(32),
        });
        harness
            .relay
            .handle_message(queued, &entry(&harness), &mut control)
            .await;

        assert!(harness.adapter.prompts.lock().unwrap().is_empty());
        let published = bodies(&harness.transport);
        assert_eq!(published.len(), 1);
        assert!(published[0].1.contains("relay error"));
        assert!(
            published[0].2.contains("inbox budget exceeded"),
            "{:?}",
            published[0].2
        );
    }
}
