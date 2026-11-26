//! Acceptance tests for the control engine, ported from
//! `tests/relay-control.test.mjs`.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use ntfy_relay_core::adapter::{
    AdapterError, Capability, NewSession, ProjectEntry, PromptRequest, Session, SessionApi,
    TargetSink,
};
use ntfy_relay_core::clock::Clock;
use ntfy_relay_inbound::control::{
    builtin_control_table, validate_control_table, Control, Decision, Run, Status,
};
use serde_json::json;

/// The futures in this suite never suspend; the deadline turns a future that
/// does suspend into a failure instead of a spin.
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

struct FakeClock(AtomicI64);

impl FakeClock {
    fn new(value: i64) -> Self {
        Self(AtomicI64::new(value))
    }

    fn advance(&self, delta: i64) {
        self.0.fetch_add(delta, Ordering::Relaxed);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct FakeState {
    created: Vec<Option<String>>,
}

#[derive(Default)]
struct FakeTargets {
    pending: Mutex<(Option<String>, i64)>,
}

impl TargetSink for FakeTargets {
    fn set_last(&self, _topic: &str, _session_id: &str, _at: i64) {}

    fn set_pending(&self, _topic: &str, session_id: Option<&str>, at: i64) {
        *self.pending.lock().unwrap() = (session_id.map(str::to_string), at);
    }
}

impl FakeTargets {
    fn pending(&self) -> Option<String> {
        self.pending.lock().unwrap().0.clone()
    }

    fn pending_at(&self) -> i64 {
        self.pending.lock().unwrap().1
    }
}

struct FakeAdapter {
    capabilities: Vec<Capability>,
    sessions: Vec<Session>,
    state: Mutex<FakeState>,
}

impl FakeAdapter {
    fn new(capabilities: Vec<Capability>) -> Self {
        Self {
            capabilities,
            sessions: Vec::new(),
            state: Mutex::new(FakeState::default()),
        }
    }

    fn with_sessions(capabilities: Vec<Capability>, sessions: Vec<Session>) -> Self {
        Self {
            capabilities,
            sessions,
            state: Mutex::new(FakeState::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().expect("adapter state")
    }
}

#[async_trait]
impl SessionApi for FakeAdapter {
    fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    async fn create_session(&self, request: NewSession<'_>) -> Result<Session, AdapterError> {
        let title = request.title.map(str::to_string);
        self.state().created.push(title.clone());
        Ok(Session {
            id: "ses_new00001".to_string(),
            title: Some(title.unwrap_or_else(|| "untitled".to_string())),
            running: false,
            updated: 0,
            project_id: request.entry.project_ids.first().cloned(),
        })
    }

    async fn list_sessions(
        &self,
        _entry: &ProjectEntry,
        limit: usize,
    ) -> Result<Vec<Session>, AdapterError> {
        Ok(self.sessions.iter().take(limit).cloned().collect())
    }

    async fn prompt(&self, _request: PromptRequest<'_>) -> Result<(), AdapterError> {
        Ok(())
    }
}

fn all_capabilities() -> Vec<Capability> {
    vec![
        Capability::SessionCreate,
        Capability::SessionList,
        Capability::SessionPin,
        Capability::SessionAuto,
        Capability::Prompt,
    ]
}

fn entry() -> ProjectEntry {
    ProjectEntry {
        topic: "ntfy-proj".to_string(),
        canonical: PathBuf::from(r"C:\a\ntfy-proj"),
        project_ids: vec!["p1".to_string()],
    }
}

fn expect_run(decision: Decision) -> Run {
    match decision {
        Decision::Run(run) => run,
        other => panic!("expected run, got {other:?}"),
    }
}

#[test]
fn unmet_capability_disables_one_action_and_reports_once() {
    let adapter = FakeAdapter::new(vec![
        Capability::SessionCreate,
        Capability::SessionList,
        Capability::SessionPin,
    ]);
    let clock = FakeClock::new(0);
    let mut control = Control::new(builtin_control_table(), &adapter, &clock);

    let disabled: Vec<(String, Vec<Capability>)> = control
        .disabled_actions()
        .map(|(verb, missing)| (verb.to_string(), missing.to_vec()))
        .collect();
    assert_eq!(disabled.len(), 1);
    assert_eq!(disabled[0].0, "auto");
    assert_eq!(disabled[0].1, vec![Capability::SessionAuto]);
    assert_eq!(
        control.disabled_warnings(),
        vec!["control action auto disabled: missing capability session.auto".to_string()]
    );

    match control.resolve("/auto", "ntfy-proj") {
        Decision::Disabled { verb, missing } => {
            assert_eq!(verb, "auto");
            assert_eq!(missing, vec![Capability::SessionAuto]);
        }
        other => panic!("expected disabled, got {other:?}"),
    }
    assert!(matches!(
        control.resolve("/sessions", "ntfy-proj"),
        Decision::Run(_)
    ));
}

#[test]
fn confirm_holds_one_action_per_topic_and_no_cancels() {
    let table = validate_control_table(&json!({
        "version": 1,
        "prefixes": ["/"],
        "caseSensitive": false,
        "actions": { "wipe": { "handler": "session.auto", "confirm": true, "ack": "wiped" } }
    }))
    .expect("table");
    let adapter = FakeAdapter::new(all_capabilities());
    let clock = FakeClock::new(0);
    let mut control = Control::new(table, &adapter, &clock);

    match control.resolve("/wipe", "t") {
        Decision::Ask { text, .. } => {
            assert_eq!(text, "confirm /wipe? answer /yes or /no within 60 s");
        }
        other => panic!("expected ask, got {other:?}"),
    }
    assert!(matches!(
        control.resolve("/wipe", "t"),
        Decision::Busy { .. }
    ));
    assert_eq!(control.resolve("/no", "t"), Decision::Cancelled);
    assert_eq!(control.resolve("/yes", "t"), Decision::None);
}

#[test]
fn yes_executes_the_held_action_once() {
    let table = validate_control_table(&json!({
        "version": 1,
        "prefixes": ["/"],
        "caseSensitive": false,
        "actions": { "wipe": { "handler": "session.auto", "confirm": true, "ack": "wiped" } }
    }))
    .expect("table");
    let adapter = FakeAdapter::new(all_capabilities());
    let clock = FakeClock::new(0);
    let mut control = Control::new(table, &adapter, &clock);

    control.resolve("/wipe", "t");
    let run = expect_run(control.resolve("/yes", "t"));
    assert!(run.confirmed);
    let text = block_on(control.execute(run, &entry())).expect("execute");
    assert_eq!(text, "wiped");
    assert_eq!(control.resolve("/yes", "t"), Decision::None);
}

#[test]
fn a_held_action_expires_and_a_late_yes_reports_expired() {
    let table = validate_control_table(&json!({
        "version": 1,
        "prefixes": ["/"],
        "caseSensitive": false,
        "actions": { "wipe": { "handler": "session.auto", "confirm": true, "ack": "wiped" } }
    }))
    .expect("table");
    let adapter = FakeAdapter::new(all_capabilities());
    let clock = FakeClock::new(1000);
    let mut control = Control::new(table, &adapter, &clock);

    control.resolve("/wipe", "t");
    clock.advance(60_001);
    assert_eq!(control.resolve("/yes", "t"), Decision::Expired);
    clock.advance(300_001);
    assert_eq!(control.resolve("/yes", "t"), Decision::None);
}

#[test]
fn cooldown_rejects_a_repeated_run_with_one_error() {
    let table = validate_control_table(&json!({
        "version": 1,
        "prefixes": ["/"],
        "caseSensitive": false,
        "actions": { "status": { "handler": "relay.status", "cooldownMs": 1000 } }
    }))
    .expect("table");
    let adapter = FakeAdapter::new(all_capabilities());
    let clock = FakeClock::new(0);
    let mut control = Control::new(table, &adapter, &clock);

    let first = expect_run(control.resolve("/status", "t"));
    block_on(control.execute(first, &entry())).expect("first run");
    let second = expect_run(control.resolve("/status", "t"));
    let error = block_on(control.execute(second, &entry())).unwrap_err();
    assert_eq!(error.to_string(), "status is on cooldown, retry in 1 s");
    clock.advance(1000);
    let third = expect_run(control.resolve("/status", "t"));
    block_on(control.execute(third, &entry())).expect("after cooldown");
}

#[test]
fn session_create_acks_id_and_project_and_targets_when_asked() {
    let adapter = FakeAdapter::new(all_capabilities());
    let targets = FakeTargets::default();
    let clock = FakeClock::new(0);
    let mut control =
        Control::new(builtin_control_table(), &adapter, &clock).with_targets(&targets);

    let run = expect_run(control.resolve("/new fix the links", "ntfy-proj"));
    let text = block_on(control.execute(run, &entry())).expect("execute");
    assert_eq!(text, "new session ses_new0 in ntfy-proj");
    assert_eq!(
        adapter.state().created[0],
        Some("fix the links".to_string())
    );
    assert_eq!(targets.pending().as_deref(), Some("ses_new00001"));
}

#[test]
fn the_pending_target_uses_the_wall_clock() {
    let adapter = FakeAdapter::with_sessions(
        all_capabilities(),
        vec![Session {
            id: "ses_old1234".to_string(),
            title: Some("Old work".to_string()),
            running: false,
            updated: 0,
            project_id: None,
        }],
    );
    let targets = FakeTargets::default();
    let clock = FakeClock::new(5);
    let wall = || 1_700_000_000_000i64;
    let mut control = Control::new(builtin_control_table(), &adapter, &clock)
        .with_targets(&targets)
        .with_hint_clock(&wall);

    let run = expect_run(control.resolve("/to Old", "ntfy-proj"));
    block_on(control.execute(run, &entry())).expect("execute");
    assert_eq!(targets.pending().as_deref(), Some("ses_old1234"));
    assert_eq!(targets.pending_at(), 1_700_000_000_000);
}

#[test]
fn session_list_honors_the_count_and_marks_running_state() {
    let adapter = FakeAdapter::with_sessions(
        all_capabilities(),
        vec![
            Session {
                id: "ses_aaaa1111".to_string(),
                title: Some("One".to_string()),
                running: false,
                updated: 0,
                project_id: None,
            },
            Session {
                id: "ses_bbbb2222".to_string(),
                title: Some("Two".to_string()),
                running: false,
                updated: 0,
                project_id: None,
            },
        ],
    );
    let clock = FakeClock::new(0);
    let mut control = Control::new(builtin_control_table(), &adapter, &clock);

    let one = expect_run(control.resolve("/sessions 1", "ntfy-proj"));
    assert_eq!(
        block_on(control.execute(one, &entry())).expect("one"),
        "1 idle One"
    );
    let all = expect_run(control.resolve("/sessions", "ntfy-proj"));
    let text = block_on(control.execute(all, &entry())).expect("all");
    assert_eq!(text.lines().count(), 2);
}

#[test]
fn help_lists_configured_verbs_and_native_commands() {
    let adapter = FakeAdapter::new(Vec::new());
    let clock = FakeClock::new(0);
    let mut control = Control::new(builtin_control_table(), &adapter, &clock);

    let run = expect_run(control.resolve("/help", "t"));
    let text = block_on(control.execute(run, &entry())).expect("help");
    assert!(
        text.contains("/new <title> (start) - unavailable: needs session.create"),
        "{text}"
    );
    assert!(
        text.contains("/steer <text> - interrupt the running turn"),
        "{text}"
    );
}

#[test]
fn status_reports_topics_queue_target_and_asks() {
    let status = || Status {
        topics: 2,
        queue: 1,
        target: Some("ntfy-proj=ses_1234".to_string()),
        asks: 3,
        uptime_seconds: None,
        sse: None,
    };
    let adapter = FakeAdapter::new(all_capabilities());
    let clock = FakeClock::new(0);
    let mut control = Control::new(builtin_control_table(), &adapter, &clock).with_status(&status);

    let run = expect_run(control.resolve("/status", "t"));
    let text = block_on(control.execute(run, &entry())).expect("status");
    assert_eq!(text, "topics 2\nqueue 1\ntarget ntfy-proj=ses_1234\nasks 3");
}

#[test]
fn status_reports_a_refused_subscription() {
    let status = || Status {
        uptime_seconds: Some(5),
        sse: Some("refused".to_string()),
        ..Default::default()
    };
    let adapter = FakeAdapter::new(all_capabilities());
    let clock = FakeClock::new(0);
    let mut control = Control::new(builtin_control_table(), &adapter, &clock).with_status(&status);
    let run = expect_run(control.resolve("/status", "t"));
    let text = block_on(control.execute(run, &entry())).expect("status");
    assert!(text.ends_with("uptime 5 s\nsse refused"), "{text}");
}

#[test]
fn a_disabled_action_is_not_a_command_except_through_the_phrase() {
    let table = validate_control_table(&json!({
        "version": 1,
        "prefixes": ["/", "hey relay"],
        "caseSensitive": false,
        "actions": { "nope": { "handler": "relay.help", "enabled": false } }
    }))
    .expect("table");
    let adapter = FakeAdapter::new(all_capabilities());
    let clock = FakeClock::new(0);
    let mut control = Control::new(table, &adapter, &clock);

    assert_eq!(control.resolve("/nope", "t"), Decision::Prompt);
    assert_eq!(
        control.resolve("hey relay nope", "t"),
        Decision::Unknown { phrase: true }
    );
}
