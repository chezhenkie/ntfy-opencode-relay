//! Permission replies resolve the ask the phone was shown.
//!
//! `reply_to_permission` takes no session api and no pin, so no path here can
//! fall back to routing; the fake permissions API records the exact session and
//! request id it was called with.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use ntfy_relay_core::adapter::{AdapterError, PermissionApi, PermissionDecision};
use ntfy_relay_core::asks::AskStore;
use ntfy_relay_inbound::permission::{reply_to_permission, PermissionError, PermissionOutcome};

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

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("inbound-permission-tests");
        let dir = base.join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn store(&self) -> AskStore {
        AskStore::open(self.0.join("asks.json"), 0).0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct FakePermissions {
    replies: Mutex<Vec<(String, String, PermissionDecision)>>,
    result: Mutex<Option<AdapterError>>,
}

impl FakePermissions {
    fn failing(error: AdapterError) -> Self {
        Self {
            replies: Mutex::new(Vec::new()),
            result: Mutex::new(Some(error)),
        }
    }

    fn calls(&self) -> Vec<(String, String, PermissionDecision)> {
        self.replies.lock().unwrap().clone()
    }
}

#[async_trait]
impl PermissionApi for FakePermissions {
    async fn list_permissions(
        &self,
        _session_id: &str,
    ) -> Result<Vec<ntfy_relay_core::adapter::Permission>, AdapterError> {
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
        match self.result.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[test]
fn a_code_less_reply_uses_the_only_ask_on_its_topic() {
    let temp = TempDir::new("newest");
    let asks = temp.store();
    let older = asks
        .register("per_1", "ses_a", "proj-a", "Older", 100)
        .unwrap();
    asks.register("per_2", "ses_other", "proj-b", "Newer", 200)
        .unwrap();
    let permissions = FakePermissions::default();

    let outcome = block_on(reply_to_permission(
        &permissions,
        &asks,
        PermissionDecision::Once,
        None,
        "proj-a",
    ))
    .unwrap();
    assert_eq!(
        permissions.calls(),
        vec![(
            "ses_a".to_string(),
            "per_1".to_string(),
            PermissionDecision::Once
        )]
    );
    assert_eq!(
        outcome.ack_body(),
        format!("approved once in Older ({}), 1 more pending", older.code)
    );
    assert_eq!(asks.count(), 1);
}

#[test]
fn two_asks_on_one_topic_need_a_code() {
    let temp = TempDir::new("ambiguous");
    let asks = temp.store();
    let first = asks
        .register("per_1", "ses_a", "proj", "Older", 100)
        .unwrap();
    let second = asks
        .register("per_2", "ses_b", "proj", "Newer", 200)
        .unwrap();
    let permissions = FakePermissions::default();
    let error = block_on(reply_to_permission(
        &permissions,
        &asks,
        PermissionDecision::Once,
        None,
        "proj",
    ))
    .unwrap_err();
    match error {
        PermissionError::Ambiguous { available } => {
            assert_eq!(available.len(), 2);
            assert!(available.iter().any(|ask| ask.code == first.code));
            assert!(available.iter().any(|ask| ask.code == second.code));
        }
        other => panic!("expected both codes, got {other:?}"),
    }
    assert!(permissions.calls().is_empty());
    assert_eq!(asks.count(), 2);
}

#[test]
fn a_code_from_another_topic_is_not_answered() {
    let temp = TempDir::new("other-topic");
    let asks = temp.store();
    let registered = asks
        .register("per_1", "ses_1", "other", "Fix links", 1)
        .unwrap();
    let permissions = FakePermissions::default();
    let error = block_on(reply_to_permission(
        &permissions,
        &asks,
        PermissionDecision::Once,
        Some(&registered.code),
        "proj",
    ))
    .unwrap_err();
    assert_eq!(error, PermissionError::NoAsk);
    assert!(permissions.calls().is_empty());
    assert_eq!(asks.count(), 1);
}

#[test]
fn a_named_code_answers_the_older_ask_with_hash_and_case() {
    let temp = TempDir::new("named");
    let asks = temp.store();
    let older = asks
        .register("per_1", "ses_a", "proj", "Older", 100)
        .unwrap();
    asks.register("per_2", "ses_b", "proj", "Newer", 200)
        .unwrap();
    let permissions = FakePermissions::default();

    let token = format!("#{}", older.code.to_uppercase());
    let outcome = block_on(reply_to_permission(
        &permissions,
        &asks,
        PermissionDecision::Reject,
        Some(&token),
        "proj",
    ))
    .unwrap();
    assert_eq!(
        permissions.calls(),
        vec![(
            "ses_a".to_string(),
            "per_1".to_string(),
            PermissionDecision::Reject
        )]
    );
    assert_eq!(
        outcome.ack_body(),
        format!("rejected in Older ({}), 1 more pending", older.code)
    );
}

#[test]
fn an_empty_registry_names_no_session() {
    let temp = TempDir::new("empty");
    let asks = temp.store();
    let permissions = FakePermissions::default();
    let error = block_on(reply_to_permission(
        &permissions,
        &asks,
        PermissionDecision::Once,
        None,
        "proj",
    ))
    .unwrap_err();
    assert_eq!(error, PermissionError::NoAsk);
    assert_eq!(error.message(), "no pending permission ask");
    assert!(permissions.calls().is_empty());
}

#[test]
fn an_unknown_code_lists_at_most_three() {
    let temp = TempDir::new("unknown");
    let asks = temp.store();
    for index in 0..4 {
        asks.register(
            &format!("per_{index}"),
            "ses_1",
            "proj",
            &format!("T{index}"),
            index as i64,
        )
        .unwrap();
    }
    let permissions = FakePermissions::default();
    let error = block_on(reply_to_permission(
        &permissions,
        &asks,
        PermissionDecision::Once,
        Some("ffff"),
        "proj",
    ))
    .unwrap_err();
    let message = error.message();
    assert!(message.starts_with("unknown permission code ffff; pending: "));
    assert_eq!(message.matches(" (").count(), 3);
    assert!(!message.contains("ses_"));
    assert!(permissions.calls().is_empty());
    assert_eq!(asks.count(), 4);
}

#[test]
fn a_missing_or_conflicting_ask_maps_to_already_answered() {
    for status in [404, 409] {
        let temp = TempDir::new(&format!("gone-{status}"));
        let asks = temp.store();
        let registered = asks
            .register("per_1", "ses_1", "proj", "Fix links", 1)
            .unwrap();
        let permissions = FakePermissions::failing(AdapterError::with_status(status, "gone"));
        let outcome = block_on(reply_to_permission(
            &permissions,
            &asks,
            PermissionDecision::Once,
            None,
            "proj",
        ))
        .unwrap();
        assert_eq!(
            outcome,
            PermissionOutcome::AlreadyAnswered {
                title: "Fix links".to_string(),
                code: registered.code,
            }
        );
        assert_eq!(asks.count(), 0);
    }
}

#[test]
fn a_400_already_answered_maps_to_already_answered() {
    for text in ["Permission already answered", "already REPLIED"] {
        let temp = TempDir::new(&format!("answered-{}", text.len()));
        let asks = temp.store();
        asks.register("per_1", "ses_1", "proj", "Fix links", 1)
            .unwrap();
        let permissions = FakePermissions::failing(AdapterError::with_status(400, text));
        let outcome = block_on(reply_to_permission(
            &permissions,
            &asks,
            PermissionDecision::Once,
            None,
            "proj",
        ))
        .unwrap();
        assert!(matches!(outcome, PermissionOutcome::AlreadyAnswered { .. }));
        assert_eq!(asks.count(), 0);
    }
}

#[test]
fn another_400_keeps_the_record() {
    let temp = TempDir::new("bad-request");
    let asks = temp.store();
    asks.register("per_1", "ses_1", "proj", "Fix links", 1)
        .unwrap();
    let permissions = FakePermissions::failing(AdapterError::with_status(400, "bad decision"));
    let error = block_on(reply_to_permission(
        &permissions,
        &asks,
        PermissionDecision::Once,
        None,
        "proj",
    ))
    .unwrap_err();
    assert_eq!(
        error,
        PermissionError::Server {
            status: 400,
            message: "bad decision".to_string()
        }
    );
    assert_eq!(asks.count(), 1, "the ask stays for a retry");
}

#[test]
fn a_server_fault_or_transport_error_keeps_the_record() {
    for status in [None, Some(500), Some(503)] {
        let temp = TempDir::new(&format!("fault-{status:?}"));
        let asks = temp.store();
        asks.register("per_1", "ses_1", "proj", "Fix links", 1)
            .unwrap();
        let error = match status {
            Some(status) => AdapterError::with_status(status, "boom"),
            None => AdapterError::new("connection reset"),
        };
        let permissions = FakePermissions::failing(error.clone());
        let reply = block_on(reply_to_permission(
            &permissions,
            &asks,
            PermissionDecision::Once,
            None,
            "proj",
        ))
        .unwrap_err();
        assert_eq!(reply, PermissionError::Adapter(error));
        assert_eq!(asks.count(), 1);
    }
}

#[test]
fn a_tombstoned_code_acks_without_a_call() {
    let temp = TempDir::new("tombstone");
    let asks = temp.store();
    let registered = asks
        .register("per_1", "ses_1", "proj", "Fix links", 1)
        .unwrap();
    asks.remove("per_1").unwrap();
    let permissions = FakePermissions::default();
    let outcome = block_on(reply_to_permission(
        &permissions,
        &asks,
        PermissionDecision::Once,
        Some(&registered.code),
        "proj",
    ))
    .unwrap();
    assert_eq!(
        outcome.ack_body(),
        format!("already answered in Fix links ({})", registered.code)
    );
    assert!(permissions.calls().is_empty());
}

#[test]
fn a_named_code_with_no_asks_is_empty_not_unknown() {
    let temp = TempDir::new("empty-named");
    let asks = temp.store();
    let permissions = FakePermissions::default();
    let error = block_on(reply_to_permission(
        &permissions,
        &asks,
        PermissionDecision::Once,
        Some("abcdef"),
        "proj",
    ))
    .unwrap_err();
    assert_eq!(error, PermissionError::NoAsk);
}
