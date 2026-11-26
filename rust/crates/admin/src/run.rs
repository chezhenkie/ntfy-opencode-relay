//! One command against the endpoint, and the plain-text rendering of each
//! answer. Output is one line per item and no color; `--json` prints the
//! response envelope unchanged.

use std::io::Write;

use ntfy_relay_control::client::ClientError;
use ntfy_relay_control::types::{Mode, ModePlanData, StatusData, TopicsData};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::cli::Command;
use crate::endpoint::{Endpoint, STOP_TIMEOUT};

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_NO_RELAY: i32 = 3;
pub const EXIT_PLATFORM: i32 = 4;

pub fn execute(
    command: &Command,
    json: bool,
    endpoint: &mut dyn Endpoint,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    if json && matches!(command, Command::Mode(_)) {
        let _ = writeln!(
            err,
            "mode runs the deploy path in the current console; drop --json"
        );
        return EXIT_USAGE;
    }
    let Some(pid) = endpoint.pid() else {
        return no_relay(err);
    };
    if !endpoint.alive(pid) {
        return no_relay(err);
    }

    let (method, path, body) = request_for(command);
    let mut reply = match endpoint.request(method, path, body.as_deref()) {
        Ok(reply) => reply,
        Err(error) => return transport(err, &error),
    };
    if reply.0 == 401 {
        // A restart changes the token; the file is read again with the retry.
        reply = match endpoint.request(method, path, body.as_deref()) {
            Ok(reply) => reply,
            Err(error) => return transport(err, &error),
        };
    }
    if reply.0 == 401 {
        return no_relay(err);
    }
    if reply.0 != 200 {
        if json {
            let _ = writeln!(out, "{}", reply.1);
        }
        let (code, message) = failure(&reply.1);
        let _ = writeln!(err, "FAIL {code}: {message}");
        return EXIT_FAILED;
    }

    if json {
        let _ = writeln!(out, "{}", reply.1);
        if matches!(command, Command::Stop) && !endpoint.wait_stopped(pid, STOP_TIMEOUT) {
            return still_running(err);
        }
        return EXIT_OK;
    }

    match command {
        Command::Topics | Command::Add(_) | Command::Remove(_) => {
            match data::<TopicsData>(&reply.1) {
                Ok(state) => {
                    print_state(out, &state.roots, &state.topics);
                    EXIT_OK
                }
                Err(message) => unexpected(err, &message),
            }
        }
        Command::Status => match data::<StatusData>(&reply.1) {
            Ok(status) => {
                let _ = writeln!(out, "pid: {}", status.pid);
                let _ = writeln!(out, "started: {}", status.started);
                print_state(out, &status.roots, &status.topics);
                EXIT_OK
            }
            Err(message) => unexpected(err, &message),
        },
        Command::Stop => {
            if !endpoint.wait_stopped(pid, STOP_TIMEOUT) {
                return still_running(err);
            }
            let _ = writeln!(out, "stopped");
            EXIT_OK
        }
        Command::Mode(_) => match data::<ModePlanData>(&reply.1) {
            Ok(plan) => {
                let _ = writeln!(out, "mode: {}", plan.mode.as_str());
                let _ = writeln!(out, "deploy: {}", plan.deploy);
                let _ = writeln!(out, "command: {}", plan.command.join(" "));
                match endpoint.run_deploy(&plan.deploy, plan.mode) {
                    Ok(code) => code,
                    Err(message) => {
                        let _ = writeln!(err, "FAIL {message}");
                        EXIT_PLATFORM
                    }
                }
            }
            Err(message) => unexpected(err, &message),
        },
    }
}

fn request_for(command: &Command) -> (&'static str, &'static str, Option<String>) {
    match command {
        Command::Topics => ("GET", "/v1/topics", None),
        Command::Add(folder) => ("POST", "/v1/topics/add", Some(path_body(folder))),
        Command::Remove(folder) => ("POST", "/v1/topics/remove", Some(path_body(folder))),
        Command::Status => ("GET", "/v1/status", None),
        Command::Stop => ("POST", "/v1/stop", None),
        Command::Mode(mode) => ("POST", "/v1/mode/plan", Some(mode_body(*mode))),
    }
}

fn path_body(folder: &str) -> String {
    serde_json::json!({ "path": folder }).to_string()
}

fn mode_body(mode: Mode) -> String {
    serde_json::json!({ "mode": mode.as_str() }).to_string()
}

fn print_state(out: &mut dyn Write, roots: &[String], topics: &[String]) {
    for root in roots {
        let _ = writeln!(out, "root: {root}");
    }
    for topic in topics {
        let _ = writeln!(out, "topic: {topic}");
    }
}

fn data<T: DeserializeOwned>(body: &str) -> Result<T, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|error| format!("response is not JSON: {error}"))?;
    let data = value
        .get("data")
        .cloned()
        .ok_or_else(|| "response has no data".to_string())?;
    serde_json::from_value(data).map_err(|error| format!("unexpected response: {error}"))
}

fn failure(body: &str) -> (String, String) {
    let error = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("error").cloned());
    match error {
        Some(error) => (
            error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string(),
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(body)
                .to_string(),
        ),
        None => ("error".to_string(), body.to_string()),
    }
}

fn no_relay(err: &mut dyn Write) -> i32 {
    let _ = writeln!(err, "no running relay");
    EXIT_NO_RELAY
}

fn transport(err: &mut dyn Write, error: &ClientError) -> i32 {
    match error {
        ClientError::NoRelay => no_relay(err),
        other => {
            let _ = writeln!(err, "FAIL {other}");
            EXIT_FAILED
        }
    }
}

fn still_running(err: &mut dyn Write) -> i32 {
    let _ = writeln!(err, "FAIL still running");
    EXIT_FAILED
}

fn unexpected(err: &mut dyn Write, message: &str) -> i32 {
    let _ = writeln!(err, "FAIL {message}");
    EXIT_FAILED
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;

    struct Fake {
        pid: Option<u32>,
        alive: bool,
        replies: VecDeque<Result<(u16, String), ClientError>>,
        calls: Vec<(String, String, Option<String>)>,
        deploy: Option<(String, Mode)>,
        deploy_code: i32,
        deploy_error: Option<String>,
        stopped: bool,
        waits: Vec<(u32, Duration)>,
    }

    impl Fake {
        fn new(replies: Vec<(u16, String)>) -> Self {
            Self {
                pid: Some(42),
                alive: true,
                replies: replies.into_iter().map(Ok).collect(),
                calls: Vec::new(),
                deploy: None,
                deploy_code: 0,
                deploy_error: None,
                stopped: true,
                waits: Vec::new(),
            }
        }
    }

    impl Endpoint for Fake {
        fn pid(&mut self) -> Option<u32> {
            self.pid
        }

        fn alive(&mut self, _pid: u32) -> bool {
            self.alive
        }

        fn request(
            &mut self,
            method: &str,
            path: &str,
            body: Option<&str>,
        ) -> Result<(u16, String), ClientError> {
            self.calls.push((
                method.to_string(),
                path.to_string(),
                body.map(str::to_string),
            ));
            self.replies
                .pop_front()
                .unwrap_or(Err(ClientError::NoRelay))
        }

        fn run_deploy(&mut self, deploy: &str, mode: Mode) -> Result<i32, String> {
            self.deploy = Some((deploy.to_string(), mode));
            match &self.deploy_error {
                Some(message) => Err(message.clone()),
                None => Ok(self.deploy_code),
            }
        }

        fn wait_stopped(&mut self, pid: u32, timeout: Duration) -> bool {
            self.waits.push((pid, timeout));
            self.stopped
        }
    }

    fn ok(data: Value) -> (u16, String) {
        (
            200,
            serde_json::json!({"ok": true, "data": data}).to_string(),
        )
    }

    fn failed(status: u16, code: &str, message: &str) -> (u16, String) {
        (
            status,
            serde_json::json!({"ok": false, "error": {"code": code, "message": message}})
                .to_string(),
        )
    }

    fn topics() -> Value {
        serde_json::json!({"roots": ["/srv"], "topics": ["app", "docs"]})
    }

    fn run(command: &Command, json: bool, fake: &mut Fake) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = execute(command, json, fake, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("utf-8 out"),
            String::from_utf8(err).expect("utf-8 err"),
        )
    }

    #[test]
    fn a_missing_or_dead_pid_is_no_relay() {
        let mut fake = Fake::new(vec![ok(topics())]);
        fake.pid = None;
        assert_eq!(run(&Command::Topics, false, &mut fake).0, EXIT_NO_RELAY);

        let mut fake = Fake::new(vec![ok(topics())]);
        fake.alive = false;
        let (code, out, err) = run(&Command::Topics, false, &mut fake);
        assert_eq!(code, EXIT_NO_RELAY);
        assert_eq!(out, "");
        assert_eq!(err, "no running relay\n");
        assert!(fake.calls.is_empty());
    }

    #[test]
    fn topics_are_one_line_per_item() {
        let mut fake = Fake::new(vec![ok(topics())]);
        let (code, out, err) = run(&Command::Topics, false, &mut fake);
        assert_eq!(code, EXIT_OK);
        assert_eq!(out, "root: /srv\ntopic: app\ntopic: docs\n");
        assert_eq!(err, "");
        assert_eq!(fake.calls, vec![("GET".into(), "/v1/topics".into(), None)]);
    }

    #[test]
    fn add_sends_the_folder_as_json() {
        let mut fake = Fake::new(vec![ok(topics())]);
        let command = Command::Add(r"C:\work\My Projects".into());
        let (code, _, _) = run(&command, false, &mut fake);
        assert_eq!(code, EXIT_OK);
        let (method, path, body) = &fake.calls[0];
        assert_eq!((method.as_str(), path.as_str()), ("POST", "/v1/topics/add"));
        let body: Value = serde_json::from_str(body.as_deref().expect("body")).expect("json");
        assert_eq!(body["path"], r"C:\work\My Projects");
    }

    #[test]
    fn a_refused_edit_reports_the_server_message() {
        let mut fake = Fake::new(vec![failed(409, "covered", "path is inside a served root")]);
        let command = Command::Add("/srv/app".into());
        let (code, out, err) = run(&command, false, &mut fake);
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(out, "");
        assert_eq!(err, "FAIL covered: path is inside a served root\n");
    }

    #[test]
    fn a_401_is_retried_once() {
        let mut fake = Fake::new(vec![failed(401, "unauthorized", "bad token"), ok(topics())]);
        let (code, out, _) = run(&Command::Topics, false, &mut fake);
        assert_eq!(code, EXIT_OK);
        assert_eq!(out, "root: /srv\ntopic: app\ntopic: docs\n");
        assert_eq!(fake.calls.len(), 2);
    }

    #[test]
    fn two_401s_are_no_relay() {
        let mut fake = Fake::new(vec![
            failed(401, "unauthorized", "bad token"),
            failed(401, "unauthorized", "bad token"),
        ]);
        let (code, _, err) = run(&Command::Topics, false, &mut fake);
        assert_eq!(code, EXIT_NO_RELAY);
        assert_eq!(err, "no running relay\n");
        assert_eq!(fake.calls.len(), 2);
    }

    #[test]
    fn json_prints_the_envelope_unchanged() {
        let (status, body) = ok(topics());
        assert_eq!(status, 200);
        let mut fake = Fake::new(vec![(status, body.clone())]);
        let (code, out, _) = run(&Command::Topics, true, &mut fake);
        assert_eq!(code, EXIT_OK);
        assert_eq!(out, format!("{body}\n"));
    }

    #[test]
    fn status_lists_the_pid_started_time_and_state() {
        let mut fake = Fake::new(vec![ok(serde_json::json!({
            "pid": 9120,
            "started": "2026-11-24T22:39:00.000Z",
            "roots": ["/srv"],
            "topics": ["app"]
        }))]);
        let (code, out, _) = run(&Command::Status, false, &mut fake);
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            out,
            "pid: 9120\nstarted: 2026-11-24T22:39:00.000Z\nroot: /srv\ntopic: app\n"
        );
    }

    #[test]
    fn stop_waits_for_the_pid_to_exit() {
        let mut fake = Fake::new(vec![(200, "{\"ok\":true}".into())]);
        let (code, out, err) = run(&Command::Stop, false, &mut fake);
        assert_eq!(code, EXIT_OK);
        assert_eq!(out, "stopped\n");
        assert_eq!(err, "");
        assert_eq!(fake.waits, vec![(42, STOP_TIMEOUT)]);
    }

    #[test]
    fn a_stop_that_survives_the_budget_fails() {
        let mut fake = Fake::new(vec![(200, "{\"ok\":true}".into())]);
        fake.stopped = false;
        let (code, out, err) = run(&Command::Stop, false, &mut fake);
        assert_eq!(code, EXIT_FAILED);
        assert_eq!(out, "");
        assert_eq!(err, "FAIL still running\n");
    }

    #[test]
    fn mode_prints_the_plan_and_runs_deploy() {
        let mut fake = Fake::new(vec![ok(serde_json::json!({
            "mode": "tailscale",
            "deploy": "/install/ntfy-deploy.exe",
            "command": ["ntfy-deploy", "mode", "tailscale"]
        }))]);
        let (code, out, _) = run(&Command::Mode(Mode::Tailscale), false, &mut fake);
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            out,
            "mode: tailscale\ndeploy: /install/ntfy-deploy.exe\ncommand: ntfy-deploy mode tailscale\n"
        );
        assert_eq!(
            fake.deploy,
            Some(("/install/ntfy-deploy.exe".into(), Mode::Tailscale))
        );
        let body: Value = serde_json::from_str(fake.calls[0].2.as_deref().expect("body")).unwrap();
        assert_eq!(body["mode"], "tailscale");
    }

    #[test]
    fn a_deploy_failure_keeps_its_exit_code() {
        let plan = ok(serde_json::json!({
            "mode": "nord",
            "deploy": "/install/ntfy-deploy.exe",
            "command": ["ntfy-deploy", "mode", "nord"]
        }));
        let mut fake = Fake::new(vec![plan]);
        fake.deploy_code = 4;
        assert_eq!(run(&Command::Mode(Mode::Nord), false, &mut fake).0, 4);
    }

    #[test]
    fn a_missing_deploy_binary_fails_as_a_platform_error() {
        let plan = ok(serde_json::json!({
            "mode": "lan",
            "deploy": "/install/ntfy-deploy.exe",
            "command": ["ntfy-deploy", "mode", "lan"]
        }));
        let mut fake = Fake::new(vec![plan]);
        fake.deploy_error = Some("cannot run /install/ntfy-deploy.exe".into());
        let (code, _, err) = run(&Command::Mode(Mode::Lan), false, &mut fake);
        assert_eq!(code, EXIT_PLATFORM);
        assert_eq!(err, "FAIL cannot run /install/ntfy-deploy.exe\n");
    }

    #[test]
    fn json_mode_is_a_usage_error() {
        let plan = ok(serde_json::json!({
            "mode": "lan",
            "deploy": "/install/ntfy-deploy.exe",
            "command": ["ntfy-deploy", "mode", "lan"]
        }));
        let mut fake = Fake::new(vec![plan]);
        let (code, out, err) = run(&Command::Mode(Mode::Lan), true, &mut fake);
        assert_eq!(code, EXIT_USAGE);
        assert_eq!(out, "");
        assert_eq!(
            err,
            "mode runs the deploy path in the current console; drop --json\n"
        );
        assert!(fake.deploy.is_none());
        assert!(fake.calls.is_empty());
    }
}
