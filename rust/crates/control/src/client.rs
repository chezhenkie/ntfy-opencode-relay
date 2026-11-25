//! Blocking client for the control endpoint, used by `ntfy-admin`.

use crate::runtime::ControlFile;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

/// One response body, capped; the endpoint answers with small JSON.
pub const MAX_BODY: usize = 1 << 20;
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    NoRelay,
    Io(String),
    Http(String),
    TooLarge,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRelay => write!(formatter, "no running relay"),
            Self::Io(message) => write!(formatter, "{message}"),
            Self::Http(message) => write!(formatter, "{message}"),
            Self::TooLarge => write!(formatter, "response is too large"),
        }
    }
}

impl std::error::Error for ClientError {}

pub struct Client {
    port: u16,
    token: String,
}

impl Client {
    /// Read the runtime file under `root`.
    pub fn connect(root: &Path) -> Result<Self, ClientError> {
        let file = ControlFile::read(root).ok_or(ClientError::NoRelay)?;
        Ok(Self {
            port: file.port,
            token: file.token,
        })
    }

    pub fn from_file(file: &ControlFile) -> Self {
        Self {
            port: file.port,
            token: file.token.clone(),
        }
    }

    /// One request over one connection. Returns the HTTP status and body.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), ClientError> {
        let mut stream =
            TcpStream::connect(("127.0.0.1", self.port)).map_err(|error| match error.kind() {
                std::io::ErrorKind::ConnectionRefused => ClientError::NoRelay,
                _ => ClientError::Io(error.to_string()),
            })?;
        let _ = stream.set_read_timeout(Some(TIMEOUT));
        let _ = stream.set_write_timeout(Some(TIMEOUT));

        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nConnection: close\r\n",
            self.token
        );
        if let Some(text) = body {
            request.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                text.len()
            ));
        }
        request.push_str("\r\n");
        if let Some(text) = body {
            request.push_str(text);
        }
        stream
            .write_all(request.as_bytes())
            .map_err(|error| ClientError::Io(error.to_string()))?;

        let mut buffer = Vec::with_capacity(1024);
        let mut limited = stream.take(MAX_BODY as u64);
        limited
            .read_to_end(&mut buffer)
            .map_err(|error| ClientError::Io(error.to_string()))?;
        split(&buffer)
    }
}

fn split(response: &[u8]) -> Result<(u16, String), ClientError> {
    let boundary = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| ClientError::Http("response has no header block".into()))?;
    let head = String::from_utf8_lossy(&response[..boundary]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| ClientError::Http("response has no status".into()))?;
    let body = String::from_utf8_lossy(&response[boundary + 4..]).to_string();
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn serve(response: &'static str) -> (u16, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = [0u8; 2048];
            let read = stream.read(&mut buffer).expect("read");
            stream
                .write_all(response.as_bytes())
                .expect("write response");
            String::from_utf8_lossy(&buffer[..read]).to_string()
        });
        (port, handle)
    }

    fn client(port: u16) -> Client {
        Client {
            port,
            token: "abc123".into(),
        }
    }

    #[test]
    fn request_carries_the_bearer_token() {
        let (port, handle) = serve(
            "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
        );
        let (status, body) = client(port)
            .request("GET", "/v1/topics", None)
            .expect("request");
        assert_eq!(status, 200);
        assert_eq!(body, "{\"ok\":true}");
        let seen = handle.join().expect("join");
        assert!(seen.starts_with("GET /v1/topics HTTP/1.1\r\n"));
        assert!(seen.contains("Authorization: Bearer abc123\r\n"));
    }

    #[test]
    fn body_carries_length_and_json() {
        let (port, handle) =
            serve("HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let text = "{\"path\":\"/srv\"}";
        let (status, body) = client(port)
            .request("POST", "/v1/topics/add", Some(text))
            .expect("request");
        assert_eq!(status, 409);
        assert_eq!(body, "");
        let seen = handle.join().expect("join");
        assert!(seen.contains(&format!("Content-Length: {}", text.len())));
        assert!(seen.ends_with(text));
    }

    #[test]
    fn refused_port_means_no_relay() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        assert_eq!(
            client(port).request("GET", "/v1/status", None),
            Err(ClientError::NoRelay)
        );
    }

    #[test]
    fn malformed_response_is_an_http_error() {
        let (port, handle) = serve("garbage");
        let error = client(port)
            .request("GET", "/v1/status", None)
            .expect_err("error");
        assert!(matches!(error, ClientError::Http(_)));
        handle.join().expect("join");
    }

    #[test]
    fn split_parses_status_and_body() {
        let (status, body) =
            split(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\n\r\nno").expect("split");
        assert_eq!(status, 401);
        assert_eq!(body, "no");
    }
}
