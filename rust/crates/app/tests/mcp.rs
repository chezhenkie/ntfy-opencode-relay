//! End-to-end MCP test: spawn `ntfy-relay mcp`, speak JSON-RPC over stdio, and
//! check tool naming, the plugin contract text, cwd-based project confinement,
//! and the publish wire against a local fake ntfy server.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

fn temp_dir(name: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("temp")
        .join("mcp-tests")
        .join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

struct FakeNtfy {
    url: String,
    seen: Arc<Mutex<Vec<u8>>>,
}

fn start_fake_ntfy() -> FakeNtfy {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake ntfy");
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let request = read_request(&mut stream);
        *sink.lock().unwrap() = request;
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}");
        let _ = stream.flush();
    });
    FakeNtfy { url, seen }
}

fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(head) = header_end(&buffer) {
            let length = content_length(&buffer[..head]).unwrap_or(0);
            if buffer.len() >= head + length {
                break;
            }
        }
    }
    buffer
}

fn header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn content_length(headers: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(headers).to_ascii_lowercase();
    text.lines().find_map(|line| {
        line.strip_prefix("content-length:")
            .and_then(|value| value.trim().parse().ok())
    })
}

struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Client {
    fn spawn(project: &Path, config: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ntfy-relay"))
            .arg("mcp")
            .current_dir(project)
            .env("NTFY_RELAY_CONFIG", config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn ntfy-relay mcp");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, frame: Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        serde_json::to_writer(&mut *stdin, &frame).expect("write frame");
        stdin.write_all(b"\n").expect("write newline");
        stdin.flush().expect("flush frame");
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        assert!(
            !line.trim().is_empty(),
            "server closed stdout without a response"
        );
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("not a JSON frame: {line}"))
    }

    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.read()
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(status) = self.child.try_wait().expect("poll child") {
                return Some(status);
            }
            thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_stdio_server_exposes_one_project_confined_send_file_tool() {
    let root = temp_dir("stdio");
    let project = root.join("mcp-proj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("note.md"), b"hello").unwrap();
    std::fs::write(root.join("outside.md"), b"nope").unwrap();
    let token = root.join("token.txt");
    std::fs::write(&token, "test-token-1234567890").unwrap();
    let ntfy = start_fake_ntfy();
    let config = root.join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "ntfy": { "server": ntfy.url, "tokenFile": token },
            "projectRoots": [project],
        }))
        .unwrap(),
    )
    .unwrap();

    let mut client = Client::spawn(&project, &config);

    let init = client.request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" },
        }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25", "{init}");
    assert_eq!(init["result"]["serverInfo"]["name"], "ntfy-relay");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    client.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    let list = client.request(2, "tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1, "{list}");
    assert_eq!(tools[0]["name"], "send_file");
    assert_eq!(tools[0]["inputSchema"]["required"], json!(["path"]));
    assert_eq!(tools[0]["inputSchema"]["additionalProperties"], false);

    let sent = client.request(
        3,
        "tools/call",
        json!({
            "name": "send_file",
            "arguments": { "path": "note.md", "message": "see readme.md" },
        }),
    );
    assert_eq!(
        sent["result"]["content"][0]["text"],
        "Sent note.md (5 bytes) to the phone."
    );
    assert_eq!(sent["result"]["isError"], false);

    let request = String::from_utf8_lossy(&ntfy.seen.lock().unwrap().clone()).into_owned();
    assert!(request.starts_with("PUT /mcp-proj?"), "{request}");
    assert!(request.contains("tags=oc-out%2Coc-manual"), "{request}");
    assert!(request.contains("message=see+readme%5B.%5Dmd"), "{request}");
    assert!(request.ends_with("hello"), "{request}");

    let refused = client.request(
        4,
        "tools/call",
        json!({
            "name": "send_file",
            "arguments": { "path": "../outside.md" },
        }),
    );
    assert_eq!(
        refused["result"]["content"][0]["text"],
        "Not sent: path is outside the project."
    );

    let unknown = client.request(5, "nope", json!({}));
    assert_eq!(unknown["error"]["code"], -32601, "{unknown}");

    client.close_stdin();
    let status = client
        .wait_for_exit(Duration::from_secs(10))
        .expect("mcp exits when stdin closes");
    assert!(status.success(), "{status:?}");
}
