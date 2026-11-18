//! Routing and prompt delivery against a fake adapter.
//!
//! Routing is derived from the live snapshot; the tests pin the order, the
//! hint windows, and the prompt outcome. Permission replies live in
//! `permission_reply.rs`; they never read a routing hint.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use ntfy_relay_core::adapter::{
    AdapterError, Capability, Delivery, NewSession, ProjectEntry, PromptFile, PromptRequest,
    Session, SessionApi, SessionSignal,
};
use ntfy_relay_inbound::prompting::{deliver_prompt, delivery_ack, DeliverError, DeliveryRequest};
use ntfy_relay_inbound::routing::{Hints, Source, Windows};

fn block_on<F: Future>(future: F) -> F::Output {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "future did not complete within 5 s"
                );
                std::thread::yield_now();
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptCall {
    session_id: String,
    text: String,
    delivery: Delivery,
    files: Vec<PromptFile>,
}

struct FakeAdapter {
    signals: Vec<SessionSignal>,
    prompts: Mutex<Vec<PromptCall>>,
    prompt_error: Option<AdapterError>,
}

impl FakeAdapter {
    fn new(signals: Vec<SessionSignal>) -> Self {
        Self {
            signals,
            prompts: Mutex::new(Vec::new()),
            prompt_error: None,
        }
    }

    fn with_prompt_error(mut self, error: AdapterError) -> Self {
        self.prompt_error = Some(error);
        self
    }

    fn prompts(&self) -> Vec<PromptCall> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl SessionApi for FakeAdapter {
    fn capabilities(&self) -> &[Capability] {
        &[
            Capability::Prompt,
            Capability::SessionList,
            Capability::SessionPin,
        ]
    }

    async fn create_session(&self, _request: NewSession<'_>) -> Result<Session, AdapterError> {
        Err(AdapterError::new("not used"))
    }

    async fn list_sessions(
        &self,
        _entry: &ProjectEntry,
        limit: usize,
    ) -> Result<Vec<Session>, AdapterError> {
        Ok(self
            .signals
            .iter()
            .take(limit)
            .map(|signal| signal.session.clone())
            .collect())
    }

    async fn session_signals(
        &self,
        _entry: &ProjectEntry,
        limit: usize,
    ) -> Result<Vec<SessionSignal>, AdapterError> {
        Ok(self.signals.iter().take(limit).cloned().collect())
    }

    async fn prompt(&self, request: PromptRequest<'_>) -> Result<(), AdapterError> {
        self.prompts.lock().unwrap().push(PromptCall {
            session_id: request.session_id.to_string(),
            text: request.text.to_string(),
            delivery: request.delivery,
            files: request.files.to_vec(),
        });
        match &self.prompt_error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

fn entry() -> ProjectEntry {
    ProjectEntry {
        topic: "oc-x".to_string(),
        canonical: PathBuf::from("/a/oc-x"),
        project_ids: vec!["p1".to_string()],
    }
}

fn signal(id: &str, updated: i64, viewed: i64, running: bool) -> SessionSignal {
    SessionSignal {
        session: Session {
            id: id.to_string(),
            title: Some(format!("Title {id}")),
            running,
            updated,
            project_id: Some("p1".to_string()),
        },
        viewed,
        parent_id: None,
    }
}

fn request<'a>(
    entry: &'a ProjectEntry,
    hints: Hints<'a>,
    text: &'a str,
    delivery: Delivery,
) -> DeliveryRequest<'a> {
    DeliveryRequest {
        entry,
        hints,
        prefer_running: true,
        windows: Windows::default(),
        now: 10_000,
        text,
        delivery,
        files: &[],
    }
}

#[test]
fn no_session_is_an_error_and_never_prompts() {
    let adapter = FakeAdapter::new(Vec::new());
    let entry = entry();
    let error = block_on(deliver_prompt(
        &adapter,
        request(&entry, Hints::default(), "hi", Delivery::Queue),
    ))
    .unwrap_err();
    assert_eq!(error, DeliverError::NoSession);
    assert!(adapter.prompts().is_empty());
}

#[test]
fn the_pending_hint_routes_one_message() {
    let adapter = FakeAdapter::new(vec![
        signal("ses_old", 100, 100, false),
        signal("ses_current", 900, 900, false),
    ]);
    let entry = entry();
    let resolved = block_on(deliver_prompt(
        &adapter,
        request(
            &entry,
            Hints {
                pending: Some("ses_old"),
                pending_at: 9_900,
                ..Hints::default()
            },
            "hi",
            Delivery::Queue,
        ),
    ))
    .unwrap();
    assert_eq!(resolved.session.id, "ses_old");
    assert_eq!(resolved.source, Source::Pending);
    assert_eq!(adapter.prompts()[0].session_id, "ses_old");
}

#[test]
fn a_stale_hint_loses_to_current_activity() {
    let adapter = FakeAdapter::new(vec![
        signal("ses_old", 100, 100, false),
        signal("ses_current", 9_000, 9_000, false),
    ]);
    let entry = entry();
    let resolved = block_on(deliver_prompt(
        &adapter,
        DeliveryRequest {
            now: 700_000,
            ..request(
                &entry,
                Hints {
                    last: Some("ses_old"),
                    last_at: 1,
                    ..Hints::default()
                },
                "hi",
                Delivery::Queue,
            )
        },
    ))
    .unwrap();
    assert_eq!(resolved.session.id, "ses_current");
    assert_eq!(resolved.source, Source::Activity);
}

#[test]
fn prompt_sends_text_delivery_and_files_to_the_resolved_session() {
    let adapter = FakeAdapter::new(vec![signal("ses_one", 1, 0, false)]);
    let entry = entry();
    let files = [PromptFile {
        path: PathBuf::from("/a/oc-x/.ntfy-inbox/1700000000-a.png"),
        name: "a.png".to_string(),
    }];
    let resolved = block_on(deliver_prompt(
        &adapter,
        DeliveryRequest {
            files: &files,
            ..request(&entry, Hints::default(), "see", Delivery::Steer)
        },
    ))
    .unwrap();
    assert_eq!(
        adapter.prompts(),
        vec![PromptCall {
            session_id: "ses_one".to_string(),
            text: "see".to_string(),
            delivery: Delivery::Steer,
            files: files.to_vec(),
        }]
    );
    let (suffix, body) = delivery_ack(&resolved.session, Delivery::Steer);
    assert_eq!(suffix, "steering");
    assert_eq!(body, "steering Title ses_one");
}

#[test]
fn prompt_failure_keeps_the_adapter_status() {
    let adapter = FakeAdapter::new(vec![signal("ses_one", 1, 0, false)])
        .with_prompt_error(AdapterError::with_status(500, "boom"));
    let entry = entry();
    let error = block_on(deliver_prompt(
        &adapter,
        request(&entry, Hints::default(), "hi", Delivery::Queue),
    ))
    .unwrap_err();
    assert_eq!(error.status(), Some(500));
    assert_eq!(error.message(), "boom");
}
