//! Loopback HTTP listener for the control endpoint.
//!
//! One request per connection, exact method and path matching, a bearer token
//! on every request, and fixed caps. Routing and policy stay with the handler;
//! this module only frames and authenticates.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::types::{error_json, ok_json, ErrorCode};

pub const HEADER_LIMIT: usize = 8 * 1024;
pub const BODY_LIMIT: usize = 16 * 1024;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Concurrent request workers. A stalled client occupies one worker, never
/// the accept loop; a fifth connection is refused at once.
pub const MAX_WORKERS: usize = 4;
/// The listener is polled every 50 ms, so a stop normally joins well inside
/// this; the deadline only guards a wedged accept.
pub const STOP_DEADLINE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub fn new(status: u16, body: String) -> Self {
        Self { status, body }
    }

    pub fn ok<T: serde::Serialize>(data: T) -> Self {
        let (status, body) = ok_json(data);
        Self { status, body }
    }

    pub fn error(code: ErrorCode, message: &str) -> Self {
        let (status, body) = error_json(code, message);
        Self { status, body }
    }

    /// An error body on a status the error code does not carry, such as 405.
    pub fn fail(status: u16, code: ErrorCode, message: &str) -> Self {
        let (_, body) = error_json(code, message);
        Self { status, body }
    }
}

/// One routed request. The listener calls it on its own thread, so an
/// implementation may block on the daemon loop.
pub trait Handler: Send + Sync + 'static {
    fn handle(&self, request: Request) -> Response;
}

pub struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    done: Option<mpsc::Receiver<()>>,
}

impl Server {
    /// Binds `127.0.0.1:0` and answers on the assigned port.
    pub fn start(token: String, handler: Arc<dyn Handler>) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let workers = Arc::new(AtomicUsize::new(0));
        let (done_tx, done) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("control-listener".to_string())
            .spawn(move || serve(&listener, &token, handler, &worker_stop, &workers, done_tx))?;
        Ok(Self {
            port,
            stop,
            thread: Some(thread),
            done: Some(done),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stops accepting, waits up to `STOP_DEADLINE` for the listener, and
    /// joins it when it exited. A listener that misses the deadline is left
    /// detached: its stop flag is already set and it owns nothing shared.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut finished = true;
        if let Some(done) = self.done.take() {
            finished = done.recv_timeout(STOP_DEADLINE).is_ok();
        }
        if let Some(thread) = self.thread.take() {
            if finished {
                let _ = thread.join();
            } else {
                drop(thread);
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve(
    listener: &TcpListener,
    token: &str,
    handler: Arc<dyn Handler>,
    stop: &AtomicBool,
    workers: &Arc<AtomicUsize>,
    done: mpsc::Sender<()>,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let active = workers.fetch_add(1, Ordering::Relaxed);
                if active >= MAX_WORKERS {
                    workers.fetch_sub(1, Ordering::Relaxed);
                    refuse_busy(stream, token);
                    continue;
                }
                let token = token.to_string();
                let handler = handler.clone();
                let worker_count = workers.clone();
                let spawned = thread::Builder::new()
                    .name("control-worker".to_string())
                    .spawn(move || {
                        serve_one(stream, &token, handler.as_ref());
                        worker_count.fetch_sub(1, Ordering::Relaxed);
                    });
                if spawned.is_err() {
                    workers.fetch_sub(1, Ordering::Relaxed);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => thread::sleep(POLL_INTERVAL),
        }
    }
    let _ = done.send(());
}

fn refuse_busy(mut stream: TcpStream, token: &str) {
    // Drain the request before closing, so the client reads this 503 instead
    // of a reset that discards it.
    let _ = stream.set_nonblocking(false);
    let deadline = Instant::now() + Duration::from_millis(250);
    let _ = read_request_by(&mut stream, token, deadline);
    let response = Response::fail(503, ErrorCode::Internal, "control endpoint is busy");
    write_response(&mut stream, &response);
}

fn serve_one(mut stream: TcpStream, token: &str, handler: &dyn Handler) {
    // An accepted socket may inherit the listener's non-blocking mode.
    let _ = stream.set_nonblocking(false);
    let response = match read_request(&mut stream, token) {
        Ok(request) => handler.handle(request),
        Err(response) => response,
    };
    write_response(&mut stream, &response);
}

struct Head {
    method: String,
    path: String,
    authorization: Option<String>,
    content_length: usize,
}

fn read_request(stream: &mut TcpStream, token: &str) -> Result<Request, Response> {
    read_request_by(stream, token, Instant::now() + REQUEST_TIMEOUT)
}

fn read_request_by(
    stream: &mut TcpStream,
    token: &str,
    deadline: Instant,
) -> Result<Request, Response> {
    let (raw, leftover) = read_head(stream, deadline)?;
    let head = parse_head(&raw)?;
    if !bearer_matches(head.authorization.as_deref(), token) {
        return Err(Response::error(ErrorCode::Unauthorized, "unauthorized"));
    }
    if head.content_length > BODY_LIMIT {
        return Err(Response::error(
            ErrorCode::Invalid,
            "body exceeds the 16384 byte cap",
        ));
    }
    let body = read_body(stream, head.content_length, deadline, leftover)?;
    Ok(Request {
        method: head.method,
        path: head.path,
        body,
    })
}

fn parse_head(raw: &[u8]) -> Result<Head, Response> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| Response::error(ErrorCode::Invalid, "request head is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let mut parts = lines.next().unwrap_or_default().split(' ');
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method.is_empty()
        || !path.starts_with('/')
        || !version.starts_with("HTTP/1.")
        || parts.next().is_some()
    {
        return Err(Response::error(
            ErrorCode::Invalid,
            "malformed request line",
        ));
    }

    let mut authorization = None;
    let mut content_length = 0usize;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(Response::error(ErrorCode::Invalid, "malformed header"));
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value
                .parse::<usize>()
                .map_err(|_| Response::error(ErrorCode::Invalid, "invalid content-length"))?;
        }
    }

    Ok(Head {
        method: method.to_string(),
        path: path.to_string(),
        authorization,
        content_length,
    })
}

fn read_head(stream: &mut TcpStream, deadline: Instant) -> Result<(Vec<u8>, Vec<u8>), Response> {
    const LIMIT: usize = HEADER_LIMIT + 4;
    let mut buffer = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    let mut scanned = 0usize;
    loop {
        if buffer.len() >= LIMIT {
            return Err(Response::error(
                ErrorCode::Invalid,
                "request header exceeds the 8192 byte cap",
            ));
        }
        let room = LIMIT - buffer.len();
        let take = chunk.len().min(room);
        let read = read_block(stream, &mut chunk[..take], deadline)?;
        if read == 0 {
            return Err(Response::error(ErrorCode::Invalid, "request is empty"));
        }
        buffer.extend_from_slice(&chunk[..read]);
        let start = scanned.saturating_sub(3);
        if let Some(index) = buffer[start..]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            let end = start + index + 4;
            if end > HEADER_LIMIT {
                return Err(Response::error(
                    ErrorCode::Invalid,
                    "request header exceeds the 8192 byte cap",
                ));
            }
            // A single read may carry the start of the body; keep it.
            let leftover = buffer.split_off(end);
            return Ok((buffer, leftover));
        }
        scanned = buffer.len();
    }
}

fn read_body(
    stream: &mut TcpStream,
    length: usize,
    deadline: Instant,
    mut body: Vec<u8>,
) -> Result<Vec<u8>, Response> {
    body.truncate(length);
    let mut chunk = [0u8; 1024];
    while body.len() < length {
        let room = length - body.len();
        let take = chunk.len().min(room);
        let read = read_block(stream, &mut chunk[..take], deadline)?;
        if read == 0 {
            return Err(Response::error(
                ErrorCode::Invalid,
                "request body ended early",
            ));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(body)
}

fn read_block(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<usize, Response> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Response::error(ErrorCode::Invalid, "request timed out"));
    }
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|_| Response::error(ErrorCode::Invalid, "request timed out"))?;
    match stream.read(buffer) {
        Ok(count) => Ok(count),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Err(Response::error(ErrorCode::Invalid, "request timed out"))
        }
        Err(_) => Err(Response::error(ErrorCode::Invalid, "request read failed")),
    }
}

fn write_response(stream: &mut TcpStream, response: &Response) {
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason(response.status),
        response.body.len()
    );
    let mut bytes = Vec::with_capacity(head.len() + response.body.len());
    bytes.extend_from_slice(head.as_bytes());
    bytes.extend_from_slice(response.body.as_bytes());
    let _ = stream.write_all(&bytes);
    let _ = stream.flush();
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// `Bearer <token>`, compared over the whole value. The length is fixed at 32
/// characters, so the early length check reveals nothing.
fn bearer_matches(header: Option<&str>, token: &str) -> bool {
    let Some(value) = header else {
        return false;
    };
    let Some((scheme, candidate)) = value.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return false;
    }
    let candidate = candidate.trim().as_bytes();
    let expected = token.as_bytes();
    if candidate.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in candidate.iter().zip(expected) {
        diff |= left ^ right;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::runtime::ControlFile;
    use crate::test_support::TempDir;
    use serde_json::json;
    use std::sync::Mutex;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[derive(Default)]
    struct Recorder {
        requests: Mutex<Vec<Request>>,
    }

    impl Handler for Recorder {
        fn handle(&self, request: Request) -> Response {
            self.requests
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(request);
            Response::ok(json!({"seen": true}))
        }
    }

    fn started(tag: &str) -> (Server, Arc<Recorder>, TempDir) {
        let recorder = Arc::new(Recorder::default());
        let server = Server::start(TOKEN.to_string(), recorder.clone()).expect("start");
        (server, recorder, TempDir::new(tag))
    }

    fn handled(recorder: &Recorder) -> usize {
        recorder
            .requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()
    }

    fn raw(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.write_all(request.as_bytes());
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    fn authorized(request: &str) -> String {
        request.replace("AUTH", &format!("Authorization: Bearer {TOKEN}"))
    }

    #[test]
    fn the_client_reads_an_ok_body() {
        let (server, recorder, root) = started("client");
        let file = ControlFile {
            pid: std::process::id(),
            port: server.port(),
            token: TOKEN.to_string(),
        };
        file.write(root.path()).expect("control file");
        let (status, body) = Client::connect(root.path())
            .expect("client")
            .request("GET", "/v1/topics", None)
            .expect("request");
        assert_eq!(status, 200);
        let value: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["seen"], true);
        assert_eq!(handled(&recorder), 1);
    }

    #[test]
    fn a_missing_or_wrong_token_is_unauthorized() {
        let (server, recorder, _root) = started("token");
        let missing = raw(
            server.port(),
            "GET /v1/topics HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        );
        assert!(missing.starts_with("HTTP/1.1 401"), "{missing}");
        let wrong = raw(
            server.port(),
            "GET /v1/topics HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer nope\r\nConnection: close\r\n\r\n",
        );
        assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");
        assert!(wrong.contains("\"code\":\"unauthorized\""), "{wrong}");
        assert_eq!(handled(&recorder), 0);
    }

    #[test]
    fn a_sleeping_client_does_not_delay_a_request_or_stop() {
        let (mut server, recorder, _root) = started("sleeping");
        let stale = TcpStream::connect(("127.0.0.1", server.port())).expect("connect");
        thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        let response = raw(
            server.port(),
            &authorized(
                "GET /v1/topics HTTP/1.1\r\nHost: 127.0.0.1\r\nAUTH\r\nConnection: close\r\n\r\n",
            ),
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(handled(&recorder), 1);
        let stopped = Instant::now();
        server.stop();
        assert!(stopped.elapsed() < Duration::from_secs(3));
        drop(stale);
    }

    struct Holding {
        state: Mutex<(usize, bool)>,
        cv: std::sync::Condvar,
    }

    impl Handler for Holding {
        fn handle(&self, _request: Request) -> Response {
            let mut state = self.state.lock().unwrap();
            state.0 += 1;
            self.cv.notify_all();
            while !state.1 {
                state = self.cv.wait(state).unwrap();
            }
            Response::ok(json!({"seen": true}))
        }
    }

    fn wait_held(holding: &Holding, count: usize) {
        let mut state = holding.state.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.0 < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "handlers never reached the hold");
            let (next, _) = holding.cv.wait_timeout(state, remaining).unwrap();
            state = next;
        }
    }

    #[test]
    fn a_fifth_connection_is_refused_while_four_workers_are_busy() {
        let holding = Arc::new(Holding {
            state: Mutex::new((0, false)),
            cv: std::sync::Condvar::new(),
        });
        let mut server = Server::start(TOKEN.to_string(), holding.clone()).expect("start");
        let mut clients = Vec::new();
        for _ in 0..MAX_WORKERS {
            let port = server.port();
            clients.push(thread::spawn(move || {
                raw(
                    port,
                    &authorized(
                        "GET /v1/topics HTTP/1.1\r\nHost: 127.0.0.1\r\nAUTH\r\nConnection: close\r\n\r\n",
                    ),
                )
            }));
        }
        wait_held(&holding, MAX_WORKERS);
        let response = raw(
            server.port(),
            &authorized(
                "GET /v1/topics HTTP/1.1\r\nHost: 127.0.0.1\r\nAUTH\r\nConnection: close\r\n\r\n",
            ),
        );
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
        {
            let mut state = holding.state.lock().unwrap();
            state.1 = true;
            holding.cv.notify_all();
        }
        for client in clients {
            let _ = client.join();
        }
        server.stop();
    }

    #[test]
    fn the_busy_response_is_written() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
            response
        });
        let (stream, _) = listener.accept().unwrap();
        refuse_busy(stream, TOKEN);
        let text = client.join().unwrap();
        assert!(text.starts_with("HTTP/1.1 503"), "{text}");
    }

    #[test]
    fn the_body_cap_is_sixteen_kibibytes() {
        let (server, recorder, _root) = started("body");
        let exact = "a".repeat(BODY_LIMIT);
        let accepted = raw(
            server.port(),
            &authorized(&format!(
                "POST /v1/topics/add HTTP/1.1\r\nHost: 127.0.0.1\r\nAUTH\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{exact}",
                BODY_LIMIT
            )),
        );
        assert!(accepted.starts_with("HTTP/1.1 200"), "{accepted}");
        assert_eq!(handled(&recorder), 1);
        assert_eq!(
            recorder
                .requests
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())[0]
                .body
                .len(),
            BODY_LIMIT
        );

        let over = "a".repeat(BODY_LIMIT + 1);
        let rejected = raw(
            server.port(),
            &authorized(&format!(
                "POST /v1/topics/add HTTP/1.1\r\nHost: 127.0.0.1\r\nAUTH\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{over}",
                BODY_LIMIT + 1
            )),
        );
        assert!(rejected.starts_with("HTTP/1.1 400"), "{rejected}");
        assert_eq!(handled(&recorder), 1);
    }

    #[test]
    fn the_header_cap_is_eight_kibibytes() {
        let (server, recorder, _root) = started("header");
        let pad = "a".repeat(HEADER_LIMIT);
        let response = raw(
            server.port(),
            &authorized(&format!(
                "GET /v1/topics HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Pad: {pad}\r\nAUTH\r\nConnection: close\r\n\r\n"
            )),
        );
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert_eq!(handled(&recorder), 0);
    }

    #[test]
    fn one_request_per_connection() {
        let (server, recorder, _root) = started("single");
        let response = raw(
            server.port(),
            &authorized(
                "GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1\r\nAUTH\r\nConnection: close\r\n\r\nGET /v1/status HTTP/1.1\r\nHost: 127.0.0.1\r\nAUTH\r\nConnection: close\r\n\r\n",
            ),
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(response.matches("HTTP/1.1").count(), 1, "{response}");
        assert_eq!(handled(&recorder), 1);
    }

    #[test]
    fn a_malformed_request_line_is_invalid() {
        let (server, recorder, _root) = started("malformed");
        let response = raw(server.port(), &authorized("GET\r\nAUTH\r\n\r\n"));
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert_eq!(handled(&recorder), 0);
    }

    #[test]
    fn tokens_compare_exactly() {
        assert!(bearer_matches(Some(&format!("Bearer {TOKEN}")), TOKEN));
        assert!(bearer_matches(Some(&format!("bearer {TOKEN}")), TOKEN));
        assert!(!bearer_matches(Some(&format!("Bearer x{TOKEN}")), TOKEN));
        assert!(!bearer_matches(Some(TOKEN), TOKEN));
        assert!(!bearer_matches(None, TOKEN));
    }

    #[test]
    fn stopping_ends_the_listener() {
        let mut server =
            Server::start(TOKEN.to_string(), Arc::new(Recorder::default())).expect("start");
        let port = server.port();
        server.stop();
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    }
}
