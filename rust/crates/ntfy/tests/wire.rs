//! Wire parity tests against `publishNtfy` (header style) and the plugin's
//! `createNtfyClient` (JSON style and file PUT).

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use ntfy_relay_ntfy::header::{Priority, MAX_MESSAGE_BYTES};
use ntfy_relay_ntfy::http::{HttpError, HttpRequest, HttpResponse, Method, Transport};
use ntfy_relay_ntfy::publish::{
    file_request, header_request, json_request, FileUpload, HeaderText, JsonText, NtfyServer,
    Publisher,
};

struct Recorder {
    status: u16,
    requests: Mutex<Vec<HttpRequest>>,
}

impl Recorder {
    fn new(status: u16) -> Arc<Self> {
        Arc::new(Self {
            status,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn taken(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().drain(..).collect()
    }
}

#[async_trait]
impl Transport for Recorder {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.requests.lock().unwrap().push(request);
        Ok(HttpResponse {
            status: self.status,
            headers: Vec::new(),
            body: Bytes::new(),
        })
    }
}

fn server() -> NtfyServer {
    NtfyServer::parse("https://ntfy.test").unwrap()
}

#[test]
fn json_request_matches_the_plugin_client() {
    let request = json_request(
        &server(),
        Some("tk_x"),
        &JsonText {
            topic: "ntfy-proj",
            title: "ntfy-proj",
            message: "queued to readme.md",
            priority: Priority::Default,
            tags: Some(&["oc-out"]),
        },
    );
    assert_eq!(request.method, Method::Post);
    assert_eq!(request.url, "https://ntfy.test/");
    assert_eq!(
        request.headers,
        vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("authorization".to_string(), "Bearer tk_x".to_string()),
        ]
    );
    // Literal from JSON.stringify in dist/ntfy-client.js: field order is
    // topic, title, message, priority, tags, and non-ASCII stays raw.
    assert_eq!(
        std::str::from_utf8(&request.body).unwrap(),
        r#"{"topic":"ntfy-proj","title":"ntfy-proj","message":"queued to readme.md","priority":3,"tags":["oc-out"]}"#
    );
}

#[test]
fn json_request_omits_absent_tags_and_keeps_utf8_raw() {
    let request = json_request(
        &server(),
        None,
        &JsonText {
            topic: "ntfy-proj",
            title: "Caf\u{e9}",
            message: "line1\nline2\t\u{1}",
            priority: Priority::High,
            tags: None,
        },
    );
    assert_eq!(request.header_value("authorization"), None);
    assert_eq!(
        std::str::from_utf8(&request.body).unwrap(),
        "{\"topic\":\"ntfy-proj\",\"title\":\"Caf\u{e9}\",\"message\":\"line1\\nline2\\t\\u0001\",\"priority\":4}"
    );
}

#[test]
fn json_request_applies_plugin_caps_on_character_boundaries() {
    let long = format!("{}\u{e9}", "a".repeat(MAX_MESSAGE_BYTES - 1));
    let request = json_request(
        &server(),
        None,
        &JsonText {
            topic: "t",
            title: "  a\nb  ",
            message: &long,
            priority: Priority::Default,
            tags: None,
        },
    );
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["title"], "a b");
    assert_eq!(
        body["message"].as_str().unwrap().len(),
        MAX_MESSAGE_BYTES - 1
    );
}

#[test]
fn header_request_matches_the_relay_client() {
    let request = header_request(
        &server(),
        Some("tk_x"),
        &HeaderText {
            topic: "ntfy-proj",
            title: "ntfy-proj - queued",
            message: "queued to readme.md",
            priority: Priority::Min,
            tags: &["oc-out"],
        },
    );
    assert_eq!(request.method, Method::Post);
    assert_eq!(request.url, "https://ntfy.test/ntfy-proj");
    assert_eq!(
        request.headers,
        vec![
            ("Authorization".to_string(), "Bearer tk_x".to_string()),
            ("Title".to_string(), "ntfy-proj - queued".to_string()),
            ("Priority".to_string(), "min".to_string()),
            ("Tags".to_string(), "oc-out".to_string()),
            (
                "content-type".to_string(),
                "text/plain;charset=UTF-8".to_string()
            ),
        ]
    );
    assert_eq!(request.body, Bytes::from_static(b"queued to readme.md"));
}

#[test]
fn header_request_encodes_unicode_titles_and_filters_tags() {
    let request = header_request(
        &server(),
        None,
        &HeaderText {
            topic: "ntfy-proj",
            title: "Caf\u{e9}",
            message: "hi",
            priority: Priority::High,
            tags: &["oc-out", "bad tag"],
        },
    );
    assert_eq!(request.header_value("Title"), Some("=?UTF-8?B?Q2Fmw6k=?="));
    assert_eq!(request.header_value("Tags"), Some("oc-out"));
    assert_eq!(request.header_value("Authorization"), None);

    let bare = header_request(
        &server(),
        None,
        &HeaderText {
            topic: "ntfy-proj",
            title: "",
            message: "hi",
            priority: Priority::Default,
            tags: &[],
        },
    );
    assert_eq!(bare.header_value("Title"), Some("-"));
    assert_eq!(bare.header_value("Tags"), None);
}

#[test]
fn file_request_matches_the_plugin_put() {
    let request = file_request(
        &server(),
        Some("tk_x"),
        &FileUpload {
            topic: "ntfy-proj",
            title: "ntfy-proj",
            message: Some("Full response attached."),
            priority: Priority::Default,
            tags: &["oc-out"],
            filename: Some("turn-ses_38d099b-1700000000.md"),
            content_type: "text/markdown; charset=utf-8",
            data: Bytes::from_static(b"# hi\n"),
        },
    );
    assert_eq!(request.method, Method::Put);
    // Query order and encoding are URLSearchParams from ntfy-client.js.
    assert_eq!(
        request.url,
        "https://ntfy.test/ntfy-proj?title=ntfy-proj&message=Full+response+attached.&priority=3&tags=oc-out&filename=turn-ses_38d099b-1700000000.md"
    );
    assert_eq!(
        request.headers,
        vec![
            (
                "content-type".to_string(),
                "text/markdown; charset=utf-8".to_string()
            ),
            ("authorization".to_string(), "Bearer tk_x".to_string()),
        ]
    );
    assert_eq!(request.body, Bytes::from_static(b"# hi\n"));
}

#[test]
fn file_request_leaves_absent_fields_out() {
    let request = file_request(
        &server(),
        None,
        &FileUpload {
            topic: "ntfy-proj",
            title: "t",
            message: None,
            priority: Priority::Min,
            tags: &[],
            filename: None,
            content_type: "application/octet-stream",
            data: Bytes::new(),
        },
    );
    assert_eq!(
        request.url,
        "https://ntfy.test/ntfy-proj?title=t&priority=1"
    );
}

#[tokio::test]
async fn publisher_accepts_2xx_and_reports_other_statuses() {
    let ok = Recorder::new(200);
    let publisher = Publisher::new(server(), Some("tk_x".into()), ok.clone());
    publisher
        .publish_json(&JsonText {
            topic: "t",
            title: "t",
            message: "m",
            priority: Priority::Default,
            tags: None,
        })
        .await
        .unwrap();
    assert_eq!(ok.taken().len(), 1);

    let denied = Recorder::new(403);
    let publisher = Publisher::new(server(), None, denied);
    let error = publisher
        .publish_header(&HeaderText {
            topic: "t",
            title: "t",
            message: "m",
            priority: Priority::Default,
            tags: &[],
        })
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "ntfy server responded with HTTP status 403"
    );
}

struct Slow;

#[async_trait]
impl Transport for Slow {
    async fn send(&self, _request: HttpRequest) -> Result<HttpResponse, HttpError> {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        Ok(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: Bytes::new(),
        })
    }
}

#[tokio::test]
async fn publisher_times_out_one_attempt() {
    let publisher = Publisher::new(server(), None, Arc::new(Slow))
        .with_timeout(std::time::Duration::from_millis(5));
    let error = publisher
        .publish_json(&JsonText {
            topic: "t",
            title: "t",
            message: "m",
            priority: Priority::Default,
            tags: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "ntfy publish timed out after 5ms");
}

#[tokio::test]
async fn publisher_caps_the_response_body() {
    let recorder = Recorder::new(200);
    let publisher = Publisher::new(server(), None, recorder.clone());
    publisher
        .publish_json(&JsonText {
            topic: "t",
            title: "t",
            message: "m",
            priority: Priority::Default,
            tags: None,
        })
        .await
        .unwrap();
    let requests = recorder.taken();
    assert_eq!(
        requests[0].max_response_bytes,
        Some(ntfy_relay_ntfy::publish::MAX_PUBLISH_RESPONSE_BYTES)
    );
}

#[test]
fn publisher_debug_redacts_the_token() {
    let publisher = Publisher::new(server(), Some("tk_secret".into()), Recorder::new(200));
    let debug = format!("{publisher:?}");
    assert!(!debug.contains("tk_secret"));
    assert!(debug.contains("<redacted>"));
}
