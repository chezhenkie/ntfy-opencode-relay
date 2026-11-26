//! Daemon side of the loopback control endpoint.
//!
//! The listener, framing, and bearer check live in `ntfy_relay_control::server`.
//! This module routes one request and forwards the calls that need the running
//! discovery, config write, or shutdown to the main loop.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use ntfy_relay_control::policy::{self, PolicyError};
use ntfy_relay_control::runtime::{new_token, ControlFile};
use ntfy_relay_control::server::{Handler, Request, Response, Server};
use ntfy_relay_control::types::{
    ErrorCode, Mode, ModePlanData, ModeRequest, PathRequest, TopicsData,
};
use ntfy_relay_core::config::Config;
use tokio::sync::mpsc as call_channel;

use crate::discovery::ProjectMap;
use crate::logger::Logger;

/// How long a request waits for the main loop before answering `internal`.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

pub enum ControlCall {
    Topics(mpsc::Sender<Response>),
    Add {
        path: String,
        reply: mpsc::Sender<Response>,
    },
    Remove {
        path: String,
        reply: mpsc::Sender<Response>,
    },
    Status(mpsc::Sender<Response>),
    Stop(mpsc::Sender<Response>),
}

pub struct AppHandler {
    calls: call_channel::Sender<ControlCall>,
    deploy: PathBuf,
}

impl AppHandler {
    fn ask(&self, make: impl FnOnce(mpsc::Sender<Response>) -> ControlCall) -> Response {
        let (reply, answer) = mpsc::channel();
        if self.calls.blocking_send(make(reply)).is_err() {
            return Response::error(ErrorCode::Internal, "relay is stopping");
        }
        match answer.recv_timeout(REPLY_TIMEOUT) {
            Ok(response) => response,
            Err(RecvTimeoutError::Timeout) => {
                Response::error(ErrorCode::Internal, "relay did not answer")
            }
            Err(RecvTimeoutError::Disconnected) => {
                Response::error(ErrorCode::Internal, "relay is stopping")
            }
        }
    }

    fn edit(&self, body: &[u8], add: bool) -> Response {
        let path = match serde_json::from_slice::<PathRequest>(body) {
            Ok(request) => request.path,
            Err(_) => {
                return Response::error(ErrorCode::Invalid, "body must be {\"path\":\"<folder>\"}")
            }
        };
        self.ask(move |reply| {
            if add {
                ControlCall::Add { path, reply }
            } else {
                ControlCall::Remove { path, reply }
            }
        })
    }
}

impl Handler for AppHandler {
    fn handle(&self, request: Request) -> Response {
        let path = request.path.as_str();
        let known = matches!(
            path,
            "/v1/topics"
                | "/v1/topics/add"
                | "/v1/topics/remove"
                | "/v1/status"
                | "/v1/stop"
                | "/v1/mode/plan"
        );
        if !known {
            return Response::error(ErrorCode::NotFound, "unknown path");
        }
        match (request.method.as_str(), path) {
            ("GET", "/v1/topics") => self.ask(ControlCall::Topics),
            ("POST", "/v1/topics/add") => self.edit(&request.body, true),
            ("POST", "/v1/topics/remove") => self.edit(&request.body, false),
            ("GET", "/v1/status") => self.ask(ControlCall::Status),
            ("POST", "/v1/stop") => self.ask(ControlCall::Stop),
            ("POST", "/v1/mode/plan") => mode_plan(&request.body, &self.deploy),
            _ => Response::fail(405, ErrorCode::Invalid, "method not allowed"),
        }
    }
}

fn mode_plan(body: &[u8], deploy: &Path) -> Response {
    let request: ModeRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(_) => {
            return Response::error(
                ErrorCode::Invalid,
                "body must be {\"mode\":\"tailscale|nord|lan\"}",
            )
        }
    };
    let Some(mode) = Mode::parse(&request.mode) else {
        return Response::error(ErrorCode::Invalid, "mode must be tailscale, nord, or lan");
    };
    Response::ok(ModePlanData {
        mode,
        deploy: deploy.to_string_lossy().into_owned(),
        command: vec![
            "ntfy-deploy".to_string(),
            "mode".to_string(),
            mode.as_str().to_string(),
        ],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootChange {
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootEdit {
    pub roots: Vec<PathBuf>,
    pub resolved: PathBuf,
    pub dropped: Vec<String>,
}

pub fn edit_roots(
    current: &[PathBuf],
    change: RootChange,
    input: &str,
    home: &Path,
) -> Result<RootEdit, (ErrorCode, String)> {
    match change {
        RootChange::Add => {
            let resolved = policy::resolve(input, home).map_err(policy_error)?;
            let dropped = current
                .iter()
                .filter(|root| policy::contains(&resolved, root))
                .map(|root| root.display().to_string())
                .collect();
            let roots = policy::add(current, resolved.clone()).map_err(policy_error)?;
            Ok(RootEdit {
                roots,
                resolved,
                dropped,
            })
        }
        RootChange::Remove => {
            let resolved = policy::resolve_for_removal(input, home).map_err(policy_error)?;
            let roots = policy::remove(current, &resolved).map_err(policy_error)?;
            Ok(RootEdit {
                roots,
                resolved,
                dropped: Vec::new(),
            })
        }
    }
}

fn policy_error(error: PolicyError) -> (ErrorCode, String) {
    (error.code(), error.message().to_string())
}

pub fn topics_data(config: &Config, projects: &ProjectMap) -> TopicsData {
    TopicsData {
        roots: config
            .project_roots
            .iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect(),
        topics: projects.topics(),
    }
}

/// 0.3.0 to 0.4.1 wrote the endpoint runtime into the command-table path. A
/// hard kill left it behind, and the next start read it as a table and refused.
/// The runtime shape is exact, so a real command table is never removed.
pub fn retire_legacy_runtime(control_path: &Path, logger: &Logger) {
    let Ok(bytes) = std::fs::read(control_path) else {
        return;
    };
    if serde_json::from_slice::<ControlFile>(&bytes).is_err() {
        return;
    }
    match std::fs::remove_file(control_path) {
        Ok(()) => logger.warn(&format!(
            "control: removed legacy runtime file {}",
            control_path.display()
        )),
        Err(error) => logger.warn(&format!(
            "control: cannot remove legacy runtime file {}: {error}",
            control_path.display()
        )),
    }
}

/// The listener plus the runtime file. The call receiver is handed to the main
/// loop, so this value only owns the stop path.
pub struct Endpoint {
    server: Server,
    root: PathBuf,
}

pub fn start(
    root: &Path,
    logger: &Logger,
) -> Result<(Endpoint, call_channel::Receiver<ControlCall>), String> {
    let token = new_token();
    let (calls_tx, calls) = call_channel::channel(8);
    let deploy = root.join(format!("ntfy-deploy{}", std::env::consts::EXE_SUFFIX));
    let handler = Arc::new(AppHandler {
        calls: calls_tx,
        deploy,
    });
    let mut server = Server::start(token.clone(), handler)
        .map_err(|error| format!("control: cannot bind 127.0.0.1: {error}"))?;
    let file = ControlFile {
        pid: std::process::id(),
        port: server.port(),
        token,
    };
    if let Err(error) = file.write(root) {
        server.stop();
        return Err(format!(
            "control: cannot write {}: {error}",
            ControlFile::path(root).display()
        ));
    }
    logger.info(&format!(
        "control listening 127.0.0.1:{} file={}",
        server.port(),
        ControlFile::path(root).display()
    ));
    Ok((
        Endpoint {
            server,
            root: root.to_path_buf(),
        },
        calls,
    ))
}

/// Posts `stop` to the endpoint under `root`. `Ok(false)` means no runtime
/// file or no listener, which is what a host without a relay looks like.
pub fn request_stop(root: &Path) -> Result<bool, String> {
    use ntfy_relay_control::client::{Client, ClientError};
    let client = match Client::connect(root) {
        Ok(client) => client,
        Err(ClientError::NoRelay) => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    match client.request("POST", "/v1/stop", None) {
        Ok((200, _)) => Ok(true),
        Ok((status, _)) => Err(format!("control stop answered {status}")),
        Err(ClientError::NoRelay) => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

impl Endpoint {
    pub fn stop(&mut self) {
        self.server.stop();
        ControlFile::remove(&self.root);
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::fs;

    fn temp(tag: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("app-control-tests")
            .join(tag);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(&root).unwrap();
        // The policy rejects `..`, so tests resolve the temporary path first.
        #[cfg(windows)]
        if let Some(rest) = root.to_string_lossy().strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
        root
    }

    fn plain() -> AppHandler {
        AppHandler {
            calls: call_channel::channel(1).0,
            deploy: PathBuf::from("/install/ntfy-deploy.exe"),
        }
    }

    async fn run(handler: &AppHandler, method: &str, path: &str, body: &[u8]) -> Response {
        let handler = AppHandler {
            calls: handler.calls.clone(),
            deploy: handler.deploy.clone(),
        };
        let method = method.to_string();
        let path = path.to_string();
        let body = body.to_vec();
        tokio::task::spawn_blocking(move || handler.handle(Request { method, path, body }))
            .await
            .expect("blocking task")
    }

    #[tokio::test]
    async fn a_call_is_forwarded_and_its_answer_returned() {
        let (tx, mut rx) = call_channel::channel(4);
        tokio::spawn(async move {
            while let Some(call) = rx.recv().await {
                if let ControlCall::Topics(reply) = call {
                    let _ = reply.send(Response::ok(TopicsData {
                        roots: vec!["/srv".into()],
                        topics: vec!["app".into()],
                    }));
                }
            }
        });
        let handler = AppHandler {
            calls: tx,
            deploy: PathBuf::from("/install/ntfy-deploy.exe"),
        };
        let response = run(&handler, "GET", "/v1/topics", b"").await;
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_str(&response.body).expect("json");
        assert_eq!(value["data"]["roots"][0], "/srv");
        assert_eq!(value["data"]["topics"][0], "app");
    }

    #[tokio::test]
    async fn unknown_paths_and_wrong_methods_differ() {
        let handler = plain();
        let unknown = run(&handler, "GET", "/v1/nope", b"").await;
        assert_eq!(unknown.status, 404);
        let wrong = run(&handler, "GET", "/v1/stop", b"").await;
        assert_eq!(wrong.status, 405);
        let value: Value = serde_json::from_str(&wrong.body).expect("json");
        assert_eq!(value["ok"], false);
    }

    #[tokio::test]
    async fn a_mode_plan_names_the_helper_and_the_mode() {
        let handler = plain();
        let response = run(&handler, "POST", "/v1/mode/plan", br#"{"mode":"nord"}"#).await;
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_str(&response.body).expect("json");
        assert_eq!(value["data"]["mode"], "nord");
        assert_eq!(value["data"]["deploy"], "/install/ntfy-deploy.exe");
        assert_eq!(
            value["data"]["command"],
            json!(["ntfy-deploy", "mode", "nord"])
        );

        let bad = run(&handler, "POST", "/v1/mode/plan", br#"{"mode":"vpn"}"#).await;
        assert_eq!(bad.status, 400);
        let missing = run(&handler, "POST", "/v1/mode/plan", b"{}").await;
        assert_eq!(missing.status, 400);
    }

    #[tokio::test]
    async fn a_missing_path_body_is_invalid() {
        let handler = plain();
        let response = run(&handler, "POST", "/v1/topics/add", b"{}").await;
        assert_eq!(response.status, 400);
        let value: Value = serde_json::from_str(&response.body).expect("json");
        assert_eq!(value["error"]["code"], "invalid");
    }

    #[test]
    fn an_add_reports_the_roots_it_covers() {
        let home = temp("edit-add");
        let root = home.join("root");
        let old = root.join("old");
        fs::create_dir_all(&old).unwrap();
        let edit = edit_roots(
            std::slice::from_ref(&old),
            RootChange::Add,
            &root.to_string_lossy(),
            &home,
        )
        .expect("edit");
        assert_eq!(edit.roots, vec![root.clone()]);
        assert_eq!(edit.dropped, vec![old.display().to_string()]);
    }

    #[test]
    fn a_duplicate_and_a_covered_root_are_conflicts() {
        let home = temp("edit-conflict");
        let root = home.join("root");
        let inner = root.join("inner");
        fs::create_dir_all(&inner).unwrap();
        let duplicate = edit_roots(
            std::slice::from_ref(&root),
            RootChange::Add,
            &root.to_string_lossy(),
            &home,
        )
        .unwrap_err();
        assert_eq!(duplicate.0, ErrorCode::Conflict);
        let covered = edit_roots(
            std::slice::from_ref(&root),
            RootChange::Add,
            &inner.to_string_lossy(),
            &home,
        )
        .unwrap_err();
        assert_eq!(covered.0, ErrorCode::Covered);
    }

    #[test]
    fn a_remove_of_a_missing_folder_still_edits_the_list() {
        let home = temp("edit-remove");
        let gone = home.join("gone");
        let edit = edit_roots(
            std::slice::from_ref(&gone),
            RootChange::Remove,
            &gone.to_string_lossy(),
            &home,
        )
        .expect("edit");
        assert!(edit.roots.is_empty());
        let unknown =
            edit_roots(&[], RootChange::Remove, &gone.to_string_lossy(), &home).unwrap_err();
        assert_eq!(unknown.0, ErrorCode::Conflict);
    }

    fn quiet_logger() -> Logger {
        Logger::new(None, ntfy_relay_core::config::LogLevel::Info, false, 1024)
    }

    #[test]
    fn an_unwritable_runtime_file_fails_the_start() {
        let root = temp("runtime-fail");
        fs::create_dir_all(ControlFile::path(&root)).unwrap();
        let result = start(&root, &quiet_logger());
        let error = match result {
            Ok(_) => panic!("start succeeded without a runtime file"),
            Err(error) => error,
        };
        assert!(error.contains("cannot write"), "{error}");
    }

    #[test]
    fn a_stop_without_a_runtime_file_is_not_an_error() {
        let root = temp("stop-missing");
        assert_eq!(request_stop(&root), Ok(false));
    }

    struct StopHandler;

    impl Handler for StopHandler {
        fn handle(&self, request: Request) -> Response {
            assert_eq!(request.path, "/v1/stop");
            Response::new(200, "{\"ok\":true}".to_string())
        }
    }

    #[test]
    fn a_stop_request_reaches_the_endpoint() {
        let root = temp("stop-live");
        let server = Server::start("tok".to_string(), Arc::new(StopHandler)).unwrap();
        ControlFile {
            pid: std::process::id(),
            port: server.port(),
            token: "tok".to_string(),
        }
        .write(&root)
        .unwrap();
        assert_eq!(request_stop(&root), Ok(true));
        ControlFile::remove(&root);
    }

    #[test]
    fn a_legacy_runtime_file_is_removed_from_the_table_path() {
        let root = temp("legacy-runtime");
        let path = root.join("ntfy-relay-control.json");
        fs::write(&path, r#"{"pid":42,"port":51234,"token":"abc"}"#).unwrap();
        retire_legacy_runtime(&path, &quiet_logger());
        assert!(!path.exists());
    }

    #[test]
    fn a_command_table_and_a_missing_file_stay() {
        let root = temp("control-table");
        let path = root.join("ntfy-relay-control.json");
        retire_legacy_runtime(&path, &quiet_logger());
        assert!(!path.exists());
        fs::write(&path, r#"{"version":1,"actions":[]}"#).unwrap();
        retire_legacy_runtime(&path, &quiet_logger());
        assert!(path.exists());
    }
}
