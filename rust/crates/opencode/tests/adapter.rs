//! Adapter behavior against a fake local API.
//!
//! Cases mirror `tests/ntfy-inbox-relay.test.mjs` for `opencode-adapter.mjs`:
//! capability gating on agent and model, title cleaning, merged and sorted
//! session lists with the running flag, and the viewed and parent signals the
//! router reads.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ntfy_relay_core::adapter::{
    AdapterError, Capability, Delivery as CoreDelivery, FormApi, NewSession, Permission,
    PermissionApi, PermissionDecision, ProjectEntry, PromptFile, PromptRequest, Session,
    SessionApi,
};
use ntfy_relay_opencode::adapter::{clean_title, OpenCodeAdapter};
use ntfy_relay_opencode::api::{
    Api, ApiError, CreateSession, Delivery, MessagePage, ModelRef, PermissionInfo,
    PromptFile as ApiPromptFile, ReplyDecision, SessionInfo, SessionTime,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CreateRecord {
    directory: String,
    agent: String,
    model: ModelRef,
    title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptRecord {
    session_id: String,
    text: String,
    delivery: Delivery,
    files: Vec<ApiPromptFile>,
}

#[derive(Default)]
struct FakeApi {
    sessions: Mutex<HashMap<String, Vec<SessionInfo>>>,
    by_id: Mutex<HashMap<String, SessionInfo>>,
    active: Mutex<Vec<String>>,
    created: Mutex<Option<SessionInfo>>,
    create_records: Mutex<Vec<CreateRecord>>,
    missing: Mutex<Vec<String>>,
    permissions: Mutex<HashMap<String, Vec<PermissionInfo>>>,
    replies: Mutex<Vec<(String, String, ReplyDecision)>>,
    forms: Mutex<Vec<(String, String, String)>>,
    prompts: Mutex<Vec<PromptRecord>>,
    prompt_failures: Mutex<VecDeque<u16>>,
}

impl FakeApi {
    fn with_sessions(rows: &[(&str, Vec<SessionInfo>)]) -> Self {
        let mut sessions = HashMap::new();
        for (project, list) in rows {
            sessions.insert(project.to_string(), list.clone());
        }
        Self {
            sessions: Mutex::new(sessions),
            ..Self::default()
        }
    }

    fn set_active(&self, ids: &[&str]) {
        *self.active.lock().unwrap() = ids.iter().map(|id| id.to_string()).collect();
    }
}

#[async_trait]
impl Api for FakeApi {
    async fn projects(&self) -> Result<Vec<ntfy_relay_opencode::api::ProjectInfo>, ApiError> {
        Ok(Vec::new())
    }

    async fn sessions(
        &self,
        project_id: &str,
        _limit: usize,
    ) -> Result<Vec<SessionInfo>, ApiError> {
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .get(project_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn session(&self, id: &str) -> Result<SessionInfo, ApiError> {
        if self
            .missing
            .lock()
            .unwrap()
            .iter()
            .any(|missing| missing == id)
        {
            return Err(ApiError::with_status(
                404,
                format!("session {id} not found"),
            ));
        }
        self.by_id
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::with_status(404, format!("session {id} not found")))
    }

    async fn active_ids(&self) -> Result<Vec<String>, ApiError> {
        Ok(self.active.lock().unwrap().clone())
    }

    async fn messages(
        &self,
        _session_id: &str,
        _limit: usize,
        _cursor: Option<&str>,
    ) -> Result<MessagePage, ApiError> {
        unimplemented!()
    }

    async fn create_session(&self, request: CreateSession<'_>) -> Result<SessionInfo, ApiError> {
        self.create_records.lock().unwrap().push(CreateRecord {
            directory: request.directory.to_string(),
            agent: request.agent.to_string(),
            model: request.model.clone(),
            title: request.title.map(str::to_string),
        });
        self.created
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| ApiError::new("no created session"))
    }

    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        delivery: Delivery,
        files: &[ApiPromptFile],
    ) -> Result<(), ApiError> {
        self.prompts.lock().unwrap().push(PromptRecord {
            session_id: session_id.to_string(),
            text: text.to_string(),
            delivery,
            files: files.to_vec(),
        });
        match self.prompt_failures.lock().unwrap().pop_front() {
            Some(status) => Err(ApiError::with_status(status, format!("HTTP {status}"))),
            None => Ok(()),
        }
    }

    async fn permissions(&self, session_id: &str) -> Result<Vec<PermissionInfo>, ApiError> {
        Ok(self
            .permissions
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn reply_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        decision: ReplyDecision,
    ) -> Result<(), ApiError> {
        self.replies.lock().unwrap().push((
            session_id.to_string(),
            permission_id.to_string(),
            decision,
        ));
        Ok(())
    }

    async fn cancel_form(
        &self,
        session_id: &str,
        form_id: &str,
        message: &str,
    ) -> Result<(), ApiError> {
        self.forms.lock().unwrap().push((
            session_id.to_string(),
            form_id.to_string(),
            message.to_string(),
        ));
        Ok(())
    }
}

fn entry(project_ids: &[&str]) -> ProjectEntry {
    ProjectEntry {
        topic: "ntfy-proj".to_string(),
        canonical: PathBuf::from("C:\\Users\\example\\projects\\ntfy-proj"),
        project_ids: project_ids.iter().map(|id| id.to_string()).collect(),
    }
}

fn info(id: &str, updated: i64, title: &str) -> SessionInfo {
    SessionInfo {
        id: id.to_string(),
        project_id: Some("p1".to_string()),
        parent_id: None,
        title: Some(title.to_string()),
        time: SessionTime { updated, viewed: 0 },
    }
}

fn adapter(api: Arc<FakeApi>, with_model: bool) -> OpenCodeAdapter {
    let model = with_model
        .then(|| ModelRef::new("opencode-go", "deepseek-v4.1-flash", Some("max")).unwrap());
    OpenCodeAdapter::new(api, Some(" build "), model)
}

#[tokio::test]
async fn capabilities_follow_agent_and_model() {
    let api = Arc::new(FakeApi::default());
    let full = adapter(api.clone(), true);
    assert!(full.capabilities().contains(&Capability::SessionCreate));
    assert!(full.capabilities().contains(&Capability::Prompt));

    let no_model = adapter(api, false);
    assert!(!no_model.capabilities().contains(&Capability::SessionCreate));
    assert!(no_model.capabilities().contains(&Capability::Prompt));
    assert_eq!(
        no_model
            .create_session(NewSession {
                entry: &entry(&["p1"]),
                title: Some("x"),
            })
            .await,
        Err(AdapterError::new(
            "session.create needs adapter.agent and adapter.model"
        ))
    );
}

#[tokio::test]
async fn create_session_sends_cleaned_title_and_config() {
    let api = Arc::new(FakeApi::default());
    *api.created.lock().unwrap() = Some(info("ses_new", 10, "prune check"));
    let adapter = adapter(api.clone(), true);

    let session = adapter
        .create_session(NewSession {
            entry: &entry(&["p1"]),
            title: Some("  prune\tcheck  "),
        })
        .await
        .unwrap();
    assert_eq!(
        session,
        Session {
            id: "ses_new".to_string(),
            title: Some("prune check".to_string()),
            running: false,
            updated: 10,
            project_id: Some("p1".to_string()),
        }
    );
    let request = api.create_records.lock().unwrap()[0].clone();
    assert_eq!(
        request,
        CreateRecord {
            directory: "C:\\Users\\example\\projects\\ntfy-proj".to_string(),
            agent: "build".to_string(),
            model: ModelRef {
                provider_id: "opencode-go".to_string(),
                id: "deepseek-v4.1-flash".to_string(),
                variant: Some("max".to_string()),
            },
            title: Some("prune check".to_string()),
        }
    );
}

#[tokio::test]
async fn create_session_rejects_an_empty_id() {
    let api = Arc::new(FakeApi::default());
    *api.created.lock().unwrap() = Some(info("", 10, "x"));
    let adapter = adapter(api, true);
    assert_eq!(
        adapter
            .create_session(NewSession {
                entry: &entry(&["p1"]),
                title: None,
            })
            .await,
        Err(AdapterError::new("session create returned no id"))
    );
}

#[tokio::test]
async fn list_sessions_merges_project_rows_sorts_and_marks_running() {
    let api = Arc::new(FakeApi::with_sessions(&[
        (
            "p1",
            vec![info("ses_a", 100, "a old"), info("ses_b", 300, "b")],
        ),
        (
            "p2",
            vec![info("ses_a", 500, "a new"), info("ses_c", 200, "c")],
        ),
    ]));
    api.set_active(&["ses_c", "ses_b"]);
    let adapter = adapter(api, true);

    let sessions = adapter
        .list_sessions(&entry(&["p1", "p2"]), 20)
        .await
        .unwrap();
    assert_eq!(
        sessions,
        vec![
            Session {
                id: "ses_a".to_string(),
                title: Some("a new".to_string()),
                running: false,
                updated: 500,
                project_id: Some("p1".to_string()),
            },
            Session {
                id: "ses_b".to_string(),
                title: Some("b".to_string()),
                running: true,
                updated: 300,
                project_id: Some("p1".to_string()),
            },
            Session {
                id: "ses_c".to_string(),
                title: Some("c".to_string()),
                running: true,
                updated: 200,
                project_id: Some("p1".to_string()),
            },
        ]
    );
}

#[tokio::test]
async fn session_signals_carry_viewed_and_parent() {
    let mut parent = info("ses_parent", 100, "parent");
    parent.time.viewed = 900;
    let mut child = info("ses_child", 300, "child");
    child.parent_id = Some("ses_parent".to_string());
    let api = Arc::new(FakeApi::with_sessions(&[("p1", vec![parent, child])]));
    api.set_active(&["ses_parent"]);
    let adapter = adapter(api, true);

    let signals = adapter.session_signals(&entry(&["p1"]), 20).await.unwrap();
    assert_eq!(signals.len(), 2);
    let parent = signals
        .iter()
        .find(|signal| signal.session.id == "ses_parent")
        .unwrap();
    assert_eq!(parent.viewed, 900);
    assert!(parent.session.running);
    assert_eq!(parent.parent_id, None);
    let child = signals
        .iter()
        .find(|signal| signal.session.id == "ses_child")
        .unwrap();
    assert_eq!(child.parent_id.as_deref(), Some("ses_parent"));
}

#[test]
fn clean_title_keeps_the_javascript_budget() {
    assert_eq!(clean_title(Some("a\u{feff}b")).as_deref(), Some("a b"));
    assert_eq!(
        clean_title(Some("a\u{0085}b")).as_deref(),
        Some("a\u{0085}b")
    );
    assert_eq!(
        clean_title(Some("x".repeat(201).as_str())),
        Some("x".repeat(200))
    );
}

#[tokio::test]
async fn get_session_maps_fields_and_missing_is_none() {
    let api = Arc::new(FakeApi::default());
    api.by_id
        .lock()
        .unwrap()
        .insert("ses_one".to_string(), info("ses_one", 42, "One"));
    let adapter = adapter(api, true);

    let session = adapter.get_session("ses_one").await.unwrap().unwrap();
    assert_eq!(session.id, "ses_one");
    assert_eq!(session.title.as_deref(), Some("One"));
    assert_eq!(session.updated, 42);
    assert_eq!(session.project_id.as_deref(), Some("p1"));
    assert!(!session.running);

    assert_eq!(adapter.get_session("ses_gone").await.unwrap(), None);
}

#[tokio::test(start_paused = true)]
async fn prompt_builds_the_node_body_and_retries_5xx() {
    let api = Arc::new(FakeApi::default());
    api.prompt_failures.lock().unwrap().push_back(503);
    let adapter = adapter(api.clone(), true);
    let path = std::env::temp_dir().join("note.md");
    let files = [PromptFile {
        path: path.clone(),
        name: "note.md".to_string(),
    }];

    adapter
        .prompt(PromptRequest {
            session_id: "ses_one",
            text: "read this",
            delivery: CoreDelivery::Queue,
            files: &files,
        })
        .await
        .unwrap();

    let prompts = api.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2, "one failure plus one retry");
    let first = &prompts[0];
    assert_eq!(first.session_id, "ses_one");
    assert_eq!(first.delivery, Delivery::Queue);
    assert_eq!(
        first.text,
        format!("read this\n\nAttached file: {}", path.display())
    );
    assert_eq!(first.files.len(), 1);
    assert_eq!(first.files[0].name, "note.md");
    assert!(first.files[0].uri.starts_with("file://"));
}

#[tokio::test(start_paused = true)]
async fn prompt_does_not_retry_4xx_and_keeps_the_status() {
    let api = Arc::new(FakeApi::default());
    api.prompt_failures.lock().unwrap().push_back(400);
    let adapter = adapter(api.clone(), true);

    let error = adapter
        .prompt(PromptRequest {
            session_id: "ses_one",
            text: "hello",
            delivery: CoreDelivery::Steer,
            files: &[],
        })
        .await
        .unwrap_err();
    assert_eq!(error.status(), Some(400));
    assert_eq!(api.prompts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn prompt_without_text_or_files_uses_the_fallback() {
    let api = Arc::new(FakeApi::default());
    let adapter = adapter(api.clone(), true);
    adapter
        .prompt(PromptRequest {
            session_id: "ses_one",
            text: "",
            delivery: CoreDelivery::Queue,
            files: &[],
        })
        .await
        .unwrap();
    assert_eq!(
        api.prompts.lock().unwrap()[0].text,
        "Please inspect the attached file."
    );
}

#[tokio::test]
async fn permissions_list_and_reply_route_through_the_adapter() {
    let api = Arc::new(FakeApi::default());
    api.permissions.lock().unwrap().insert(
        "ses_one".to_string(),
        vec![
            PermissionInfo {
                id: "per_2".to_string(),
            },
            PermissionInfo {
                id: "per_1".to_string(),
            },
        ],
    );
    let adapter = adapter(api.clone(), true);

    let requests = adapter.list_permissions("ses_one").await.unwrap();
    assert_eq!(
        requests,
        vec![
            Permission {
                id: "per_2".to_string()
            },
            Permission {
                id: "per_1".to_string()
            },
        ]
    );

    adapter
        .reply_permission("ses_one", "per_1", PermissionDecision::Always)
        .await
        .unwrap();
    assert_eq!(
        api.replies.lock().unwrap().as_slice(),
        &[(
            "ses_one".to_string(),
            "per_1".to_string(),
            ReplyDecision::Always
        )]
    );
}

#[tokio::test]
async fn a_form_cancel_carries_the_free_text() {
    let api = Arc::new(FakeApi::default());
    let adapter = adapter(api.clone(), true);
    adapter
        .cancel_form("ses_one", "frm_1", "keep both, any order")
        .await
        .unwrap();
    assert_eq!(
        api.forms.lock().unwrap().as_slice(),
        &[(
            "ses_one".to_string(),
            "frm_1".to_string(),
            "keep both, any order".to_string()
        )]
    );
}
