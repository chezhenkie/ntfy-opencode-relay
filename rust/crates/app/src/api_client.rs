//! OpenCode client that survives a service restart.
//!
//! The shared service rewrites `~/.local/state/opencode/service.json` on every
//! start, usually with a new port and password. A client built once goes stale:
//! the old port refuses connections and the old password answers 401. The
//! wrapper re-reads its source after either failure and retries the call once.
//! Errors the server actually answered keep their retry policy in the adapter,
//! so a 500 is not repeated here.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use ntfy_relay_core::config::Config;
use ntfy_relay_opencode::api::{
    Api, ApiError, BaseUrl, BasicAuth, CreateSession, Delivery, HttpApi, MessagePage,
    PermissionInfo, ProjectInfo, PromptFile, ReplyDecision, ServiceRegistration, SessionInfo,
};
use ntfy_relay_opencode::event_stream::{EventSource, EventStream};

use crate::lock::pid_alive;
use crate::logger::Logger;

/// One client and the credentials it was built from. The credentials are kept
/// so a reload can tell a real service change from a repeated read.
struct Current {
    api: Arc<dyn Api>,
    events: Arc<dyn EventSource>,
    base: BaseUrl,
    auth: BasicAuth,
}

pub struct BuiltApi {
    pub api: Arc<dyn Api>,
    pub events: Arc<dyn EventSource>,
    pub base: BaseUrl,
    pub auth: BasicAuth,
}

/// Credentials plus a ready client. Tests substitute a fake for the file reader.
pub trait ApiSource: Send + Sync {
    fn build(&self) -> Result<BuiltApi, String>;
}

/// Where the base URL and password come from.
enum CredentialsSource {
    /// Shared registration, rewritten by every service start.
    Registration(PathBuf),
    /// Explicit `opencode.baseUrl`; the password file can still rotate.
    ServiceFile { base: BaseUrl, path: PathBuf },
}

pub struct ApiFactory {
    source: CredentialsSource,
    timeout: Duration,
}

impl ApiFactory {
    pub fn from_config(config: &Config, home: &Path) -> Result<Self, String> {
        let timeout = Duration::from_millis(config.opencode.timeout_ms);
        let source = match config.opencode.base_url.as_deref() {
            Some(raw) => CredentialsSource::ServiceFile {
                base: BaseUrl::parse(raw)
                    .map_err(|error| format!("config: opencode.baseUrl: {error}"))?,
                path: home.join(".config").join("opencode").join("service.json"),
            },
            None => CredentialsSource::Registration(ServiceRegistration::state_path(home)),
        };
        Ok(Self { source, timeout })
    }

    fn credentials(&self) -> Result<(BaseUrl, BasicAuth), String> {
        match &self.source {
            CredentialsSource::Registration(path) => {
                let registration = ServiceRegistration::load(path)
                    .map_err(|error| error.message().to_string())?
                    .ok_or_else(|| {
                        format!(
                            "no service registration at {}; start the OpenCode service or set opencode.baseUrl",
                            path.display()
                        )
                    })?;
                if !pid_alive(registration.pid()) {
                    return Err(format!(
                        "service registration {} points at pid {}, which is not running; start the OpenCode service or set opencode.baseUrl",
                        path.display(),
                        registration.pid()
                    ));
                }
                Ok((registration.base_url().clone(), registration.auth()))
            }
            CredentialsSource::ServiceFile { base, path } => {
                let bytes = fs::read(path)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
                let auth = BasicAuth::from_service_json(&bytes)
                    .map_err(|error| error.message().to_string())?;
                Ok((base.clone(), auth))
            }
        }
    }
}

impl ApiSource for ApiFactory {
    fn build(&self) -> Result<BuiltApi, String> {
        let (base, auth) = self.credentials()?;
        let client = Arc::new(
            HttpApi::new(base.clone())
                .map_err(|error| error.message().to_string())?
                .with_auth(auth.clone())
                .with_timeout(self.timeout),
        );
        Ok(BuiltApi {
            api: client.clone(),
            events: client,
            base,
            auth,
        })
    }
}

/// A transport error or a rejected password can be fixed by a reload; anything
/// the server answered otherwise is a real API error.
fn retryable(error: &ApiError) -> bool {
    matches!(error.code(), None | Some(401))
}

pub struct RediscoveringApi {
    current: RwLock<Current>,
    source: Arc<dyn ApiSource>,
    logger: Logger,
}

impl RediscoveringApi {
    pub fn new(source: Arc<dyn ApiSource>, logger: Logger) -> Result<Arc<Self>, String> {
        let built = source.build()?;
        Ok(Arc::new(Self {
            current: RwLock::new(Current {
                api: built.api,
                events: built.events,
                base: built.base,
                auth: built.auth,
            }),
            source,
            logger,
        }))
    }

    fn current(&self) -> Arc<dyn Api> {
        self.current
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .api
            .clone()
    }

    fn current_events(&self) -> Arc<dyn EventSource> {
        self.current
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .events
            .clone()
    }

    /// Returns the new base URL when the client changed, `None` when the
    /// source still yields the same credentials.
    fn reload(&self) -> Result<Option<String>, String> {
        let built = self.source.build()?;
        let mut current = self
            .current
            .write()
            .unwrap_or_else(|poison| poison.into_inner());
        if current.base == built.base && current.auth == built.auth {
            return Ok(None);
        }
        *current = Current {
            api: built.api,
            events: built.events,
            base: built.base,
            auth: built.auth,
        };
        Ok(Some(current.base.as_str().to_string()))
    }
}

macro_rules! attempt_read {
    ($self:ident, $method:ident $(, $arg:expr)*) => {{
        let first = $self.current();
        match first.$method($($arg),*).await {
            Err(error) if retryable(&error) => match $self.reload() {
                Ok(Some(url)) => {
                    $self
                        .logger
                        .info(&format!("opencode service rediscovered url={url}"));
                    let second = $self.current();
                    second.$method($($arg),*).await
                }
                Ok(None) => Err(error),
                Err(reload) => {
                    $self
                        .logger
                        .warn(&format!("opencode rediscovery failed: {reload}"));
                    Err(error)
                }
            },
            result => result,
        }
    }};
}

/// Writes refresh the client but repeat only a request the server rejected.
/// A transport error is ambiguous: the caller's retry policy decides, and its
/// next attempt already uses the fresh client.
macro_rules! attempt_write {
    ($self:ident, $method:ident $(, $arg:expr)*) => {{
        let first = $self.current();
        match first.$method($($arg),*).await {
            Err(error) if retryable(&error) => {
                let refreshed = match $self.reload() {
                    Ok(Some(url)) => {
                        $self
                            .logger
                            .info(&format!("opencode service rediscovered url={url}"));
                        true
                    }
                    Ok(None) => false,
                    Err(reload) => {
                        $self
                            .logger
                            .warn(&format!("opencode rediscovery failed: {reload}"));
                        false
                    }
                };
                if refreshed && error.code() == Some(401) {
                    let second = $self.current();
                    second.$method($($arg),*).await
                } else {
                    Err(error)
                }
            }
            result => result,
        }
    }};
}

#[async_trait]
impl Api for RediscoveringApi {
    async fn projects(&self) -> Result<Vec<ProjectInfo>, ApiError> {
        attempt_read!(self, projects)
    }

    async fn sessions(&self, project_id: &str, limit: usize) -> Result<Vec<SessionInfo>, ApiError> {
        attempt_read!(self, sessions, project_id, limit)
    }

    async fn session(&self, id: &str) -> Result<SessionInfo, ApiError> {
        attempt_read!(self, session, id)
    }

    async fn active_ids(&self) -> Result<Vec<String>, ApiError> {
        attempt_read!(self, active_ids)
    }

    async fn messages(
        &self,
        session_id: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<MessagePage, ApiError> {
        attempt_read!(self, messages, session_id, limit, cursor)
    }

    async fn create_session(&self, request: CreateSession<'_>) -> Result<SessionInfo, ApiError> {
        attempt_write!(self, create_session, request)
    }

    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        delivery: Delivery,
        files: &[PromptFile],
    ) -> Result<(), ApiError> {
        attempt_write!(self, prompt, session_id, text, delivery, files)
    }

    async fn permissions(&self, session_id: &str) -> Result<Vec<PermissionInfo>, ApiError> {
        attempt_read!(self, permissions, session_id)
    }

    async fn reply_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        decision: ReplyDecision,
    ) -> Result<(), ApiError> {
        attempt_write!(self, reply_permission, session_id, permission_id, decision)
    }
}

/// A stream subscribe behaves like a read: a transport error or a rejected
/// password is retried once on a fresh client.
#[async_trait]
impl EventSource for RediscoveringApi {
    async fn subscribe(&self, types: &[&str]) -> Result<EventStream, ApiError> {
        let first = self.current_events();
        match first.subscribe(types).await {
            Err(error) if retryable(&error) => match self.reload() {
                Ok(Some(url)) => {
                    self.logger
                        .info(&format!("opencode service rediscovered url={url}"));
                    let second = self.current_events();
                    second.subscribe(types).await
                }
                Ok(None) => Err(error),
                Err(reload) => {
                    self.logger
                        .warn(&format!("opencode rediscovery failed: {reload}"));
                    Err(error)
                }
            },
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntfy_relay_core::config::LogLevel;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn logger() -> Logger {
        Logger::new(None, LogLevel::Error, false, 1024)
    }

    /// Fails the first `failures` calls; the counter is shared across rebuilt
    /// clients so a retry that lands on a fresh client succeeds.
    struct FakeApi {
        failures: usize,
        status: Option<u16>,
        calls: Arc<AtomicUsize>,
    }

    impl FakeApi {
        fn outcome(&self) -> Result<(), ApiError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.failures {
                return Err(match self.status {
                    Some(status) => ApiError::with_status(status, "fake failure"),
                    None => ApiError::new("fake transport failure"),
                });
            }
            Ok(())
        }
    }

    #[async_trait]
    impl Api for FakeApi {
        async fn projects(&self) -> Result<Vec<ProjectInfo>, ApiError> {
            self.outcome()?;
            Ok(vec![ProjectInfo {
                id: "p1".to_string(),
                canonical: None,
                time: Default::default(),
            }])
        }

        async fn sessions(
            &self,
            _project_id: &str,
            _limit: usize,
        ) -> Result<Vec<SessionInfo>, ApiError> {
            unimplemented!()
        }

        async fn session(&self, _id: &str) -> Result<SessionInfo, ApiError> {
            unimplemented!()
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
            self.outcome()
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
    }

    struct FakeEvents;

    #[async_trait]
    impl EventSource for FakeEvents {
        async fn subscribe(&self, _types: &[&str]) -> Result<EventStream, ApiError> {
            Err(ApiError::new("fake stream"))
        }
    }

    struct FakeSource {
        builds: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
        failures: usize,
        status: Option<u16>,
        same_base: bool,
        same_password: bool,
        fail_after_first: bool,
    }

    impl ApiSource for FakeSource {
        fn build(&self) -> Result<BuiltApi, String> {
            let index = self.builds.fetch_add(1, Ordering::SeqCst);
            if self.fail_after_first && index > 0 {
                return Err("registration is being rewritten".to_string());
            }
            let base = if self.same_base {
                "http://127.0.0.1:4100".to_string()
            } else {
                format!("http://127.0.0.1:{}", 4100 + index)
            };
            let password = if self.same_password {
                "pw".to_string()
            } else {
                format!("pw-{index}")
            };
            Ok(BuiltApi {
                api: Arc::new(FakeApi {
                    failures: self.failures,
                    status: self.status,
                    calls: self.calls.clone(),
                }),
                events: Arc::new(FakeEvents),
                base: BaseUrl::parse(&base).unwrap(),
                auth: BasicAuth::opencode(&password),
            })
        }
    }

    #[derive(Default)]
    struct Setup {
        failures: usize,
        status: Option<u16>,
        same_base: bool,
        same_password: bool,
        fail_after_first: bool,
    }

    fn setup(config: Setup) -> (Arc<RediscoveringApi>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let builds = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(FakeSource {
            builds: builds.clone(),
            calls: calls.clone(),
            failures: config.failures,
            status: config.status,
            same_base: config.same_base,
            same_password: config.same_password,
            fail_after_first: config.fail_after_first,
        });
        let wrapper = RediscoveringApi::new(source, logger()).unwrap();
        (wrapper, builds, calls)
    }

    #[test]
    fn registration_reload_picks_up_a_rewritten_url_and_password() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("app-api-client-tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("service.json");
        let pid = std::process::id();
        let write = |url: &str, password: &str| {
            fs::write(
                &path,
                format!(r#"{{"url":"{url}","pid":{pid},"password":"{password}"}}"#),
            )
            .unwrap();
        };

        write("http://127.0.0.1:4100", "pw-one");
        let factory = ApiFactory {
            source: CredentialsSource::Registration(path.clone()),
            timeout: Duration::from_secs(1),
        };
        let (base, auth) = factory.credentials().unwrap();
        assert_eq!(base.as_str(), "http://127.0.0.1:4100");
        assert_eq!(auth.header(), BasicAuth::opencode("pw-one").header());

        write("http://127.0.0.1:4200", "pw-two");
        let (base, auth) = factory.credentials().unwrap();
        assert_eq!(base.as_str(), "http://127.0.0.1:4200");
        assert_eq!(auth.header(), BasicAuth::opencode("pw-two").header());
    }

    #[test]
    fn transport_errors_and_401_reload_but_answered_errors_do_not() {
        assert!(retryable(&ApiError::new("connection refused")));
        assert!(retryable(&ApiError::with_status(401, "unauthorized")));
        assert!(!retryable(&ApiError::with_status(404, "not found")));
        assert!(!retryable(&ApiError::with_status(500, "server error")));
    }

    #[tokio::test]
    async fn a_transport_error_reloads_once_and_retries() {
        let (wrapper, builds, calls) = setup(Setup {
            failures: 1,
            ..Setup::default()
        });
        let projects = wrapper.projects().await.unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_transport_error_on_a_prompt_refreshes_without_repeating_the_post() {
        let (wrapper, builds, calls) = setup(Setup {
            failures: 1,
            ..Setup::default()
        });
        let error = wrapper
            .prompt("s1", "hi", Delivery::Queue, &[])
            .await
            .unwrap_err();
        assert_eq!(error.message(), "fake transport failure");
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // The next attempt lands on the fresh client without another reload.
        wrapper
            .prompt("s1", "hi", Delivery::Queue, &[])
            .await
            .unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_rejected_prompt_is_retried_on_the_fresh_client() {
        let (wrapper, builds, calls) = setup(Setup {
            failures: 1,
            status: Some(401),
            same_base: true,
            ..Setup::default()
        });
        wrapper
            .prompt("s1", "hi", Delivery::Queue, &[])
            .await
            .unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_answered_server_error_is_not_retried_here() {
        let (wrapper, builds, calls) = setup(Setup {
            failures: 1,
            status: Some(500),
            ..Setup::default()
        });
        let error = wrapper.projects().await.unwrap_err();
        assert_eq!(error.code(), Some(500));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_rejected_password_reloads_even_when_the_url_is_unchanged() {
        let (wrapper, builds, calls) = setup(Setup {
            failures: 1,
            status: Some(401),
            same_base: true,
            ..Setup::default()
        });
        let projects = wrapper.projects().await.unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unchanged_credentials_keep_the_original_error_and_skip_the_retry() {
        let (wrapper, builds, calls) = setup(Setup {
            failures: 1,
            same_base: true,
            same_password: true,
            ..Setup::default()
        });
        let error = wrapper.projects().await.unwrap_err();
        assert_eq!(error.message(), "fake transport failure");
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_reload_keeps_the_original_error() {
        let (wrapper, builds, calls) = setup(Setup {
            failures: 1,
            fail_after_first: true,
            ..Setup::default()
        });
        let error = wrapper.projects().await.unwrap_err();
        assert_eq!(error.message(), "fake transport failure");
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
