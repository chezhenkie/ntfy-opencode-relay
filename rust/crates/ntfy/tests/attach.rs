//! Attachment fetch parity tests: HTTPS on every hop, the redirect cap, the
//! declared and streamed size caps, and inbox naming.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use ntfy_relay_ntfy::attach::{fetch_attachment, AttachError, Attachment, MAX_REDIRECTS};
use ntfy_relay_ntfy::http::{HttpError, HttpRequest, HttpResponse, Transport};

enum Reply {
    Response(HttpResponse),
    TooLarge(u64),
}

struct Fake {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl Fake {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().drain(..).collect()
    }
}

#[async_trait]
impl Transport for Fake {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.requests.lock().unwrap().push(request);
        match self.replies.lock().unwrap().pop_front() {
            Some(Reply::Response(response)) => Ok(response),
            Some(Reply::TooLarge(limit)) => Err(HttpError::TooLarge { limit }),
            None => Err(HttpError::Transport("no reply".into())),
        }
    }
}

fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Reply {
    Reply::Response(HttpResponse {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        body: Bytes::copy_from_slice(body),
    })
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("ntfy-crate-tests");
        let dir = base.join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn attachment(url: &str, name: Option<&str>, size: Option<u64>) -> Attachment {
    Attachment {
        url: url.to_string(),
        name: name.map(str::to_string),
        size,
    }
}

#[tokio::test]
async fn saves_the_body_under_a_sanitized_name() {
    let temp = TempDir::new("save");
    let transport = Fake::new(vec![response(200, &[], b"hello")]);
    let saved = fetch_attachment(
        &transport,
        &attachment("https://ntfy.test/file", Some("../../a b.txt"), Some(5)),
        &["ntfy.test".to_string()],
        "https://ntfy.test",
        &temp.path().join(".ntfy-inbox"),
        1024,
        1_700_000_000_000,
    )
    .await
    .unwrap();

    assert_eq!(saved.name, "ab.txt");
    assert_eq!(saved.bytes, 5);
    assert_eq!(
        saved.path.file_name().unwrap().to_str().unwrap(),
        "1700000000-ab.txt"
    );
    assert_eq!(std::fs::read(&saved.path).unwrap(), b"hello");

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://ntfy.test/file");
    assert_eq!(requests[0].max_response_bytes, Some(1024));
}

#[tokio::test]
async fn a_name_collision_falls_back_to_milliseconds() {
    let temp = TempDir::new("collision");
    let inbox = temp.path().join(".ntfy-inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    std::fs::write(inbox.join("1700000000-x.txt"), b"old").unwrap();

    let transport = Fake::new(vec![response(200, &[], b"new")]);
    let saved = fetch_attachment(
        &transport,
        &attachment("https://ntfy.test/file", Some("x.txt"), Some(3)),
        &["ntfy.test".to_string()],
        "https://ntfy.test",
        &inbox,
        1024,
        1_700_000_000_123,
    )
    .await
    .unwrap();
    assert_eq!(
        saved.path.file_name().unwrap().to_str().unwrap(),
        "1700000000123-x.txt"
    );
    assert_eq!(
        std::fs::read(inbox.join("1700000000-x.txt")).unwrap(),
        b"old"
    );
}

#[tokio::test]
async fn both_standard_names_taken_yields_the_next_free_name() {
    let temp = TempDir::new("collision-counter");
    let inbox = temp.path().join(".ntfy-inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    std::fs::write(inbox.join("1700000000-x.txt"), b"first").unwrap();
    std::fs::write(inbox.join("1700000000123-x.txt"), b"second").unwrap();

    let transport = Fake::new(vec![response(200, &[], b"third")]);
    let saved = fetch_attachment(
        &transport,
        &attachment("https://ntfy.test/file", Some("x.txt"), Some(5)),
        &["ntfy.test".to_string()],
        "https://ntfy.test",
        &inbox,
        1024,
        1_700_000_000_123,
    )
    .await
    .unwrap();
    assert_eq!(
        saved.path.file_name().unwrap().to_str().unwrap(),
        "1700000000123-1-x.txt"
    );
    assert_eq!(
        std::fs::read(inbox.join("1700000000123-x.txt")).unwrap(),
        b"second"
    );
}

#[tokio::test]
async fn plain_http_is_allowed_on_loopback_only() {
    let temp = TempDir::new("http-loopback");
    let transport = Fake::new(vec![response(200, &[], b"hi")]);
    let saved = fetch_attachment(
        &transport,
        &attachment("http://127.0.0.1:8810/file", Some("x.txt"), Some(2)),
        &["127.0.0.1".to_string()],
        "http://127.0.0.1:8810",
        temp.path(),
        1024,
        1,
    )
    .await
    .unwrap();
    assert_eq!(saved.bytes, 2);
    assert_eq!(transport.requests().len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn saved_attachments_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new("mode");
    let inbox = temp.path().join(".ntfy-inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let probe = inbox.join("probe");
    std::fs::write(&probe, b"x").unwrap();
    // A Windows mount in WSL ignores chmod and reports 0777 for everything.
    if std::fs::metadata(&probe).unwrap().permissions().mode() & 0o022 != 0 {
        return;
    }
    let transport = Fake::new(vec![response(200, &[], b"hi")]);
    let saved = fetch_attachment(
        &transport,
        &attachment("https://ntfy.test/file", Some("x.txt"), Some(2)),
        &["ntfy.test".to_string()],
        "https://ntfy.test",
        &inbox,
        1024,
        1,
    )
    .await
    .unwrap();
    let mode = std::fs::metadata(&saved.path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[tokio::test]
async fn plain_http_is_rejected_before_any_request() {
    let temp = TempDir::new("http");
    let transport = Fake::new(vec![]);
    let error = fetch_attachment(
        &transport,
        &attachment("http://ntfy.test/a.png", None, None),
        &["ntfy.test".to_string()],
        "https://ntfy.test",
        temp.path(),
        1024,
        1,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, AttachError::TransportRequired));
    assert!(transport.requests().is_empty());
}

#[tokio::test]
async fn a_redirect_downgrade_to_http_is_rejected() {
    let temp = TempDir::new("downgrade");
    let transport = Fake::new(vec![response(
        302,
        &[("location", "http://ntfy.test/a.png")],
        b"",
    )]);
    let error = fetch_attachment(
        &transport,
        &attachment("https://ntfy.test/a.png", None, None),
        &["ntfy.test".to_string()],
        "https://ntfy.test",
        temp.path(),
        1024,
        1,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, AttachError::TransportRequired));
    assert_eq!(transport.requests().len(), 1);
}

#[tokio::test]
async fn more_than_three_redirects_are_rejected() {
    let temp = TempDir::new("redirects");
    let replies = (0..=MAX_REDIRECTS)
        .map(|index| {
            let location = format!("https://ntfy.test/hop-{index}");
            Reply::Response(HttpResponse {
                status: 301,
                headers: vec![("location".to_string(), location)],
                body: Bytes::new(),
            })
        })
        .collect();
    let transport = Fake::new(replies);
    let error = fetch_attachment(
        &transport,
        &attachment("https://ntfy.test/a.png", None, None),
        &["ntfy.test".to_string()],
        "https://ntfy.test",
        temp.path(),
        1024,
        1,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, AttachError::Redirects));
    assert_eq!(transport.requests().len(), MAX_REDIRECTS + 1);
}

#[tokio::test]
async fn a_declared_size_over_the_cap_fails_without_a_request() {
    let temp = TempDir::new("declared");
    let transport = Fake::new(vec![]);
    let error = fetch_attachment(
        &transport,
        &attachment("https://ntfy.test/a.png", None, Some(64)),
        &["ntfy.test".to_string()],
        "https://ntfy.test",
        temp.path(),
        8,
        1,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        AttachError::DeclaredTooLarge {
            declared: 64,
            limit: 8
        }
    ));
    assert!(transport.requests().is_empty());
}

#[tokio::test]
async fn an_oversize_body_leaves_no_file() {
    let temp = TempDir::new("oversize");
    let inbox = temp.path().join(".ntfy-inbox");
    let transport = Fake::new(vec![response(200, &[], &[0u8; 64])]);
    let error = fetch_attachment(
        &transport,
        &attachment("https://ntfy.test/a.bin", Some("a.bin"), None),
        &["ntfy.test".to_string()],
        "https://ntfy.test",
        &inbox,
        8,
        1,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, AttachError::TooLarge { limit: 8 }));
    assert!(!inbox.exists() || std::fs::read_dir(&inbox).unwrap().next().is_none());
}

#[tokio::test]
async fn a_transport_cap_maps_to_the_same_error() {
    let temp = TempDir::new("transport-cap");
    let transport = Fake::new(vec![Reply::TooLarge(8)]);
    let error = fetch_attachment(
        &transport,
        &attachment("https://ntfy.test/a.bin", None, None),
        &["ntfy.test".to_string()],
        "https://ntfy.test",
        temp.path(),
        8,
        1,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, AttachError::TooLarge { limit: 8 }));
}

#[tokio::test]
async fn a_declared_size_mismatch_is_rejected() {
    let temp = TempDir::new("mismatch");
    let transport = Fake::new(vec![response(200, &[], b"abcd")]);
    let error = fetch_attachment(
        &transport,
        &attachment("https://ntfy.test/a.bin", None, Some(5)),
        &["ntfy.test".to_string()],
        "https://ntfy.test",
        temp.path(),
        1024,
        1,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        AttachError::SizeMismatch {
            declared: 5,
            actual: 4
        }
    ));
}

#[tokio::test]
async fn an_http_error_status_is_reported() {
    let temp = TempDir::new("status");
    let transport = Fake::new(vec![response(403, &[], b"")]);
    let error = fetch_attachment(
        &transport,
        &attachment("https://ntfy.test/a.bin", None, None),
        &["ntfy.test".to_string()],
        "https://ntfy.test",
        temp.path(),
        1024,
        1,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, AttachError::Status(403)));
}

#[tokio::test]
async fn a_public_ntfy_host_refuses_another_attachment_host() {
    let temp = TempDir::new("host-mismatch");
    for url in [
        "https://internal.test/a.png",
        "http://127.0.0.1:8810/a.png",
        "http://10.0.0.5/a.png",
    ] {
        let transport = Fake::new(vec![]);
        let error = fetch_attachment(
            &transport,
            &attachment(url, None, None),
            &["ntfy.test".to_string()],
            "https://ntfy.test",
            temp.path(),
            1024,
            1,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                error,
                AttachError::HostMismatch | AttachError::TransportRequired
            ),
            "{url}: {error}"
        );
        assert!(transport.requests().is_empty(), "{url}");
    }
}

#[tokio::test]
async fn a_loopback_ntfy_host_may_fetch_a_loopback_attachment() {
    let temp = TempDir::new("host-loopback");
    let transport = Fake::new(vec![response(200, &[], b"hi")]);
    let saved = fetch_attachment(
        &transport,
        &attachment("http://127.0.0.1:8810/file", Some("x.txt"), Some(2)),
        &["127.0.0.1".to_string()],
        "http://127.0.0.1:8810",
        temp.path(),
        1024,
        1,
    )
    .await
    .unwrap();
    assert_eq!(saved.bytes, 2);
    assert_eq!(transport.requests().len(), 1);
}

#[tokio::test]
async fn a_deployment_host_attachment_is_fetched_through_the_server_base() {
    let temp = TempDir::new("host-deployment");
    let transport = Fake::new(vec![response(200, &[], b"hi")]);
    let saved = fetch_attachment(
        &transport,
        &attachment(
            "http://100.64.0.10:8810/file/abc",
            Some("mesh.png"),
            Some(2),
        ),
        &["127.0.0.1".to_string(), "100.64.0.10".to_string()],
        "http://127.0.0.1:8810",
        temp.path(),
        1024,
        1,
    )
    .await
    .unwrap();
    assert_eq!(saved.bytes, 2);
    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "http://127.0.0.1:8810/file/abc");
}

#[tokio::test]
async fn a_redirect_to_another_host_is_refused() {
    let temp = TempDir::new("host-redirect");
    let transport = Fake::new(vec![response(
        302,
        &[("location", "https://elsewhere.test/a.png")],
        b"",
    )]);
    let error = fetch_attachment(
        &transport,
        &attachment("https://ntfy.test/a.png", None, None),
        &["ntfy.test".to_string()],
        "https://ntfy.test",
        temp.path(),
        1024,
        1,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, AttachError::HostMismatch));
    assert_eq!(transport.requests().len(), 1);
}
