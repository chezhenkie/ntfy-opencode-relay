//! Session to project topic resolution.
//!
//! One global subscriber sees every server location, so the plugin's
//! per-location directory gate becomes a lookup: the envelope location when
//! present, otherwise the session's project canonical. A session lookup is
//! bounded to five seconds and failures fail closed (no publish) but are cached
//! briefly so a broken server is not hammered once per event.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use ntfy_relay_core::topic::{topic_for_project, ProjectRoots, TopicOwners};
use ntfy_relay_opencode::api::Api;

use crate::scope::{canonical, within_roots};

pub const SESSION_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
pub const PROJECT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
pub const PROJECT_CACHE_MS: i64 = 60_000;
pub const NEGATIVE_CACHE_MS: i64 = 60_000;
/// Session metadata kept per process; oldest entries are evicted first.
pub const MAX_SESSIONS: usize = 2_048;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMeta {
    pub topic: Option<String>,
    pub canonical: Option<PathBuf>,
    pub parent_id: Option<String>,
    /// Session title at lookup time; the permission ack and registry use it.
    pub title: Option<String>,
    /// The session's project is known to sit outside `projectRoots` or lost the
    /// topic to another canonical.
    pub denied: bool,
    /// A lookup failed for a transient reason; the event must stay unmarked so
    /// a later backfill can recover it.
    pub retryable: bool,
}

impl SessionMeta {
    pub fn is_child(&self) -> bool {
        self.parent_id.is_some()
    }
}

/// First 8 characters, matching the control list's short id.
pub fn short_session_id(id: &str) -> String {
    id.chars().take(8).collect()
}

struct Entry {
    meta: SessionMeta,
    looked_up_at: i64,
}

pub struct SessionResolver {
    roots: Arc<RwLock<ProjectRoots>>,
    owners: Arc<RwLock<TopicOwners>>,
    /// Last root list read from the shared state; the scope for every lookup.
    current: Vec<PathBuf>,
    seen_version: u64,
    projects: HashMap<String, Option<PathBuf>>,
    projects_at: i64,
    sessions: HashMap<String, Entry>,
    order: VecDeque<String>,
}

impl SessionResolver {
    pub fn new(roots: Arc<RwLock<ProjectRoots>>, owners: Arc<RwLock<TopicOwners>>) -> Self {
        let (current, seen_version) = match roots.read() {
            Ok(live) => (live.roots().to_vec(), live.version()),
            Err(_) => (Vec::new(), 0),
        };
        Self {
            roots,
            owners,
            current,
            seen_version,
            projects: HashMap::new(),
            projects_at: 0,
            sessions: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// The live roots; used when a caller needs the current scope without a
    /// session lookup, such as a cold backfill.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots
            .read()
            .map(|live| live.roots().to_vec())
            .unwrap_or_else(|_| self.current.clone())
    }

    /// Shared root list for a lane task that resolves its own sessions.
    pub fn roots_handle(&self) -> Arc<RwLock<ProjectRoots>> {
        self.roots.clone()
    }

    /// Shared topic ownership for a lane task that resolves its own sessions.
    pub fn owners_handle(&self) -> Arc<RwLock<TopicOwners>> {
        self.owners.clone()
    }

    /// Drops the session cache when the shared root list moved. A denied
    /// session may now be served and a resolved one may now be out of scope,
    /// so every cached decision is stale.
    fn refresh_roots(&mut self) {
        let live = self
            .roots
            .read()
            .unwrap_or_else(|poison| poison.into_inner());
        if live.version() == self.seen_version {
            return;
        }
        self.seen_version = live.version();
        self.current = live.roots().to_vec();
        self.sessions.clear();
        self.order.clear();
    }

    /// True only for the canonical that owns the topic in the shared map.
    fn owned(&self, topic: &str, canonical: &Path) -> bool {
        self.owners
            .read()
            .map(|owners| owners.owns(topic, canonical))
            .unwrap_or(false)
    }

    /// Records the flat `session.created` payload. A parent ID marks a child
    /// session; the project still comes from the event location or a lookup.
    pub fn note_created(&mut self, session_id: &str, parent_id: Option<&str>) {
        let entry = self
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| Entry {
                meta: SessionMeta::default(),
                looked_up_at: 0,
            });
        if let Some(parent_id) = parent_id {
            entry.meta.parent_id = Some(parent_id.to_string());
        }
        if !self.order.iter().any(|id| id == session_id) {
            self.order.push_back(session_id.to_string());
            self.evict();
        }
    }

    pub fn forget(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
    }

    /// Resolves one session. `location` is the envelope directory when the
    /// frame carried one.
    pub async fn meta(
        &mut self,
        api: &dyn Api,
        session_id: &str,
        location: Option<&str>,
        now: i64,
    ) -> SessionMeta {
        self.meta_with(api, session_id, location, now, false).await
    }

    /// Same as `meta`, but `force` ignores the negative cache. Backfill uses it
    /// so a lookup that failed when the event arrived can still recover.
    pub async fn meta_with(
        &mut self,
        api: &dyn Api,
        session_id: &str,
        location: Option<&str>,
        now: i64,
        force: bool,
    ) -> SessionMeta {
        self.refresh_roots();
        let (mut meta, looked_up_at) = {
            let entry = self
                .sessions
                .entry(session_id.to_string())
                .or_insert_with(|| Entry {
                    meta: SessionMeta::default(),
                    looked_up_at: 0,
                });
            (entry.meta.clone(), entry.looked_up_at)
        };

        if let Some(location) = location {
            let observed = self.location_meta(location);
            if observed.denied {
                meta.denied = true;
                meta.topic = None;
                meta.canonical = None;
            } else if observed.topic.is_some() {
                meta.topic = observed.topic;
                meta.canonical = observed.canonical;
                meta.denied = false;
            }
        }

        if meta.topic.is_some() || meta.denied {
            self.store(session_id, meta.clone(), looked_up_at);
            return meta;
        }
        if !force && looked_up_at != 0 && now - looked_up_at < NEGATIVE_CACHE_MS {
            return meta;
        }

        let info = tokio::time::timeout(SESSION_LOOKUP_TIMEOUT, api.session(session_id)).await;
        let info = match info {
            Ok(Ok(info)) => info,
            Ok(Err(_)) | Err(_) => {
                meta.retryable = true;
                self.store(session_id, meta.clone(), now);
                return meta;
            }
        };
        meta.retryable = false;
        if info.parent_id.is_some() {
            meta.parent_id = info.parent_id.clone();
        }
        meta.title = info.title.clone().filter(|title| !title.trim().is_empty());
        if let Some(project_id) = info.project_id.as_deref() {
            let (canonical, query_failed) = self.project_canonical(api, project_id, now).await;
            if query_failed {
                meta.retryable = true;
            }
            if let Some(canonical) = canonical {
                if within_roots(&canonical, &self.current) {
                    if let Some(topic) = topic_for_project(&canonical.to_string_lossy()) {
                        if self.owned(topic, &canonical) {
                            meta.topic = Some(topic.to_string());
                            meta.canonical = Some(canonical);
                            meta.denied = false;
                        } else {
                            meta.topic = None;
                            meta.canonical = None;
                            meta.denied = true;
                        }
                    } else {
                        meta.denied = true;
                    }
                } else {
                    meta.denied = true;
                }
            }
        }
        self.store(session_id, meta.clone(), now);
        meta
    }

    fn location_meta(&self, directory: &str) -> SessionMeta {
        let path = PathBuf::from(directory);
        if !within_roots(&path, &self.current) {
            return SessionMeta {
                denied: true,
                ..SessionMeta::default()
            };
        }
        match topic_for_project(directory) {
            Some(topic) => {
                let canonical = canonical(&path).unwrap_or(path);
                if !self.owned(topic, &canonical) {
                    return SessionMeta {
                        denied: true,
                        ..SessionMeta::default()
                    };
                }
                SessionMeta {
                    topic: Some(topic.to_string()),
                    canonical: Some(canonical),
                    ..SessionMeta::default()
                }
            }
            None => SessionMeta {
                denied: true,
                ..SessionMeta::default()
            },
        }
    }

    async fn project_canonical(
        &mut self,
        api: &dyn Api,
        project_id: &str,
        now: i64,
    ) -> (Option<PathBuf>, bool) {
        let stale = now - self.projects_at >= PROJECT_CACHE_MS;
        let mut failed = false;
        if stale || !self.projects.contains_key(project_id) {
            match tokio::time::timeout(PROJECT_LOOKUP_TIMEOUT, api.projects()).await {
                Ok(Ok(projects)) => {
                    self.projects = projects
                        .into_iter()
                        .map(|project| (project.id, project.canonical.map(PathBuf::from)))
                        .collect();
                    self.projects_at = now;
                }
                Ok(Err(_)) | Err(_) => failed = true,
            }
        }
        (self.projects.get(project_id).cloned().flatten(), failed)
    }

    fn store(&mut self, session_id: &str, meta: SessionMeta, looked_up_at: i64) {
        let is_new = self
            .sessions
            .insert(session_id.to_string(), Entry { meta, looked_up_at })
            .is_none();
        if is_new {
            self.order.push_back(session_id.to_string());
            self.evict();
        }
    }

    fn evict(&mut self) {
        while self.order.len() > MAX_SESSIONS {
            if let Some(oldest) = self.order.pop_front() {
                self.sessions.remove(&oldest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ntfy_relay_opencode::api::{
        ApiError, CreateSession, Delivery, MessagePage, PermissionInfo, ProjectInfo, PromptFile,
        ReplyDecision, SessionInfo,
    };

    struct FakeApi {
        projects: Vec<ProjectInfo>,
        sessions: HashMap<String, SessionInfo>,
    }

    #[async_trait]
    impl Api for FakeApi {
        async fn projects(&self) -> Result<Vec<ProjectInfo>, ApiError> {
            Ok(self.projects.clone())
        }

        async fn sessions(
            &self,
            _project_id: &str,
            _limit: usize,
        ) -> Result<Vec<SessionInfo>, ApiError> {
            unimplemented!()
        }

        async fn session(&self, id: &str) -> Result<SessionInfo, ApiError> {
            self.sessions
                .get(id)
                .cloned()
                .ok_or_else(|| ApiError::with_status(404, "not found"))
        }

        async fn active_ids(&self) -> Result<Vec<String>, ApiError> {
            unimplemented!()
        }

        async fn messages(
            &self,
            _session_id: &str,
            _limit: usize,
            _cursor: Option<&str>,
        ) -> Result<MessagePage, ApiError> {
            unimplemented!()
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

    fn root(name: &str) -> PathBuf {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("outbound-topic-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn project(id: &str, canonical: &std::path::Path) -> ProjectInfo {
        ProjectInfo {
            id: id.to_string(),
            canonical: Some(canonical.to_string_lossy().into_owned()),
            time: Default::default(),
        }
    }

    fn owners(entries: &[(&str, PathBuf)]) -> Arc<RwLock<TopicOwners>> {
        let mut map = TopicOwners::new();
        for (topic, canonical) in entries {
            map.insert(topic, canonical.clone());
        }
        Arc::new(RwLock::new(map))
    }

    fn live_roots(roots: Vec<PathBuf>) -> Arc<RwLock<ProjectRoots>> {
        Arc::new(RwLock::new(ProjectRoots::new(roots)))
    }

    fn session(id: &str, project: &str, parent: Option<&str>) -> SessionInfo {
        SessionInfo {
            id: id.to_string(),
            project_id: Some(project.to_string()),
            parent_id: parent.map(str::to_string),
            title: None,
            time: Default::default(),
        }
    }

    #[tokio::test]
    async fn an_envelope_location_resolves_without_a_lookup() {
        let root = root("location");
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let api = FakeApi {
            projects: Vec::new(),
            sessions: HashMap::new(),
        };
        let mut resolver = SessionResolver::new(
            live_roots(vec![root.clone()]),
            owners(&[("proj", proj.clone())]),
        );
        let meta = resolver
            .meta(&api, "ses_1", Some(&proj.to_string_lossy()), 1_000)
            .await;
        assert_eq!(meta.topic.as_deref(), Some("proj"));
        assert!(!meta.denied);
    }

    #[tokio::test]
    async fn a_losing_canonical_is_denied() {
        let root = root("owner");
        let winner = root.join("a/proj");
        let loser = root.join("b/proj");
        std::fs::create_dir_all(&winner).unwrap();
        std::fs::create_dir_all(&loser).unwrap();
        let api = FakeApi {
            projects: Vec::new(),
            sessions: HashMap::new(),
        };
        let mut resolver = SessionResolver::new(
            live_roots(vec![root.clone()]),
            owners(&[("proj", winner.clone())]),
        );
        let winner_meta = resolver
            .meta(&api, "ses_win", Some(&winner.to_string_lossy()), 1_000)
            .await;
        assert_eq!(winner_meta.topic.as_deref(), Some("proj"));
        let loser_meta = resolver
            .meta(&api, "ses_lose", Some(&loser.to_string_lossy()), 1_000)
            .await;
        assert!(loser_meta.denied);
        assert!(loser_meta.topic.is_none());
    }

    #[tokio::test]
    async fn a_location_outside_the_roots_is_denied() {
        let root = root("outside");
        let outside = root.join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let api = FakeApi {
            projects: Vec::new(),
            sessions: HashMap::new(),
        };
        let mut resolver = SessionResolver::new(live_roots(vec![root.join("proj")]), owners(&[]));
        let meta = resolver
            .meta(&api, "ses_1", Some(&outside.to_string_lossy()), 1_000)
            .await;
        assert!(meta.denied);
        assert!(meta.topic.is_none());
    }

    #[tokio::test]
    async fn a_session_lookup_uses_the_project_canonical() {
        let root = root("lookup");
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let mut sessions = HashMap::new();
        sessions.insert("ses_1".to_string(), session("ses_1", "p1", None));
        let api = FakeApi {
            projects: vec![project("p1", &proj)],
            sessions,
        };
        let mut resolver =
            SessionResolver::new(live_roots(vec![root]), owners(&[("proj", proj.clone())]));
        let meta = resolver.meta(&api, "ses_1", None, 1_000).await;
        assert_eq!(meta.topic.as_deref(), Some("proj"));
        assert!(!meta.is_child());
    }

    #[tokio::test]
    async fn a_parent_id_from_session_created_marks_a_child() {
        let root = root("child");
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let api = FakeApi {
            projects: Vec::new(),
            sessions: HashMap::new(),
        };
        let mut resolver =
            SessionResolver::new(live_roots(vec![root]), owners(&[("proj", proj.clone())]));
        resolver.note_created("ses_child", Some("ses_root"));
        let meta = resolver
            .meta(&api, "ses_child", Some(&proj.to_string_lossy()), 1_000)
            .await;
        assert!(meta.is_child());
        assert_eq!(meta.topic.as_deref(), Some("proj"));
    }

    #[tokio::test]
    async fn a_project_outside_the_roots_is_denied() {
        let root = root("denied");
        let proj = root.join("proj");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let mut sessions = HashMap::new();
        sessions.insert("ses_1".to_string(), session("ses_1", "p1", None));
        let api = FakeApi {
            projects: vec![project("p1", &elsewhere)],
            sessions,
        };
        let mut resolver =
            SessionResolver::new(live_roots(vec![proj.clone()]), owners(&[("proj", proj)]));
        let meta = resolver.meta(&api, "ses_1", None, 1_000).await;
        assert!(meta.denied);
    }

    #[tokio::test]
    async fn a_root_add_ends_a_sticky_denial() {
        let root = root("root-add");
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let mut sessions = HashMap::new();
        sessions.insert("ses_1".to_string(), session("ses_1", "p1", None));
        let api = FakeApi {
            projects: vec![project("p1", &proj)],
            sessions,
        };
        // The denial is cached at 1_000; the negative cache would hide a retry
        // for a minute, so only the shared root change can clear it here.
        let live = live_roots(vec![root.join("nowhere")]);
        let mut resolver = SessionResolver::new(live.clone(), owners(&[("proj", proj.clone())]));
        let first = resolver.meta(&api, "ses_1", None, 1_000).await;
        assert!(first.denied);

        live.write().unwrap().set(vec![root.clone()]);
        let second = resolver.meta(&api, "ses_1", None, 2_000).await;
        assert_eq!(second.topic.as_deref(), Some("proj"));
        assert!(!second.denied);
    }

    #[tokio::test]
    async fn a_root_removal_ends_a_resolved_topic() {
        let root = root("root-remove");
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let mut sessions = HashMap::new();
        sessions.insert("ses_1".to_string(), session("ses_1", "p1", None));
        let api = FakeApi {
            projects: vec![project("p1", &proj)],
            sessions,
        };
        let live = live_roots(vec![root.clone()]);
        let mut resolver = SessionResolver::new(live.clone(), owners(&[("proj", proj.clone())]));
        let first = resolver.meta(&api, "ses_1", None, 1_000).await;
        assert_eq!(first.topic.as_deref(), Some("proj"));

        live.write().unwrap().set(Vec::new());
        let second = resolver.meta(&api, "ses_1", None, 2_000).await;
        assert!(second.denied);
        assert!(second.topic.is_none());
    }

    #[tokio::test]
    async fn a_failed_lookup_is_cached_briefly() {
        let root = root("missing");
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let api = FakeApi {
            projects: Vec::new(),
            sessions: HashMap::new(),
        };
        let mut resolver = SessionResolver::new(live_roots(vec![root]), owners(&[]));
        let first = resolver.meta(&api, "ses_1", None, 1_000).await;
        assert!(first.topic.is_none());
        assert!(first.retryable, "a failed lookup is retryable");
        let second = resolver.meta(&api, "ses_1", None, 2_000).await;
        assert!(second.topic.is_none());
    }

    #[tokio::test]
    async fn a_forced_lookup_ignores_the_negative_cache() {
        let root = root("forced");
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let api = FakeApi {
            projects: Vec::new(),
            sessions: HashMap::new(),
        };
        let mut resolver = SessionResolver::new(live_roots(vec![root]), owners(&[("proj", proj)]));
        let first = resolver.meta(&api, "ses_1", None, 1_000).await;
        assert!(first.retryable);
        let forced = resolver.meta_with(&api, "ses_1", None, 1_100, true).await;
        assert!(forced.topic.is_none());
        assert!(forced.retryable);
    }
}
