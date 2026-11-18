//! `ntfy-relay mcp`: the stdio MCP server with one `send_file` tool.
//!
//! OpenCode spawns this process per directory with the workspace as its
//! working directory, so the current directory is the project root and the
//! folder name is the topic. The tool calls `outbound::files::send_file`
//! directly: no daemon, no lock, no state, and a send is never dropped by the
//! outbound queue. Policy code is shared with the daemon, so the two paths
//! cannot drift.
//!
//! Protocol: newline-delimited JSON-RPC 2.0 on stdin and stdout. Only stdout
//! carries protocol frames; startup errors go to stderr.

use std::io;
use std::path::PathBuf;

use ntfy_relay_core::config::load_config;
use ntfy_relay_ntfy::http::{ReqwestTransport, Transport};
use ntfy_relay_ntfy::publish::{NtfyServer, Publisher};
use ntfy_relay_outbound::files::{send_file, SendFileInput};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Tool name. OpenCode renders it as `<server>_<tool>`, so a server named
/// `ntfy` exposes `ntfy_send_file`, the name the plugin used.
pub const TOOL_NAME: &str = "send_file";

/// Same contract text as the plugin tool.
pub const TOOL_DESCRIPTION: &str = "Send an existing local file to the user's phone through ntfy. Use only when the user explicitly asks for a file on the phone.";

/// Revisions accepted in `initialize`; anything else negotiates down to the
/// newest revision before the 2025-11-25 one.
const PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const FALLBACK_VERSION: &str = "2025-06-18";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// One request line above this is rejected and the server exits.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

struct Server {
    publisher: Publisher,
    roots: Vec<PathBuf>,
    project_root: PathBuf,
}

/// Reads the config and token, resolves the project from the process working
/// directory, and serves until stdin closes.
pub async fn run(config_path: PathBuf) -> Result<(), String> {
    let config = load_config(&config_path).map_err(|error| error.to_string())?;
    let server = NtfyServer::parse(&config.ntfy.server)
        .map_err(|error| format!("config: ntfy.server: {error}"))?;
    let token = std::fs::read_to_string(&config.ntfy.token_file)
        .map_err(|error| {
            format!(
                "cannot read token file {}: {error}",
                config.ntfy.token_file.display()
            )
        })?
        .trim()
        .to_string();
    if token.len() < 10 {
        return Err(format!(
            "token file {} is empty or too short",
            config.ntfy.token_file.display()
        ));
    }
    let project_root = std::env::current_dir().map_err(|error| {
        format!("cannot resolve the project directory from the working directory: {error}")
    })?;
    let transport: std::sync::Arc<dyn Transport> =
        std::sync::Arc::new(ReqwestTransport::new().map_err(|error| error.to_string())?);
    let server = Server {
        publisher: Publisher::new(server, Some(token), transport),
        roots: config.project_roots,
        project_root,
    };
    serve(
        &server,
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
    .map_err(|error| format!("mcp: {error}"))
}

async fn serve<R, W>(server: &Server, mut reader: R, mut writer: W) -> io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let line = match read_line_bounded(&mut reader).await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                let response = error_response(Value::Null, INVALID_REQUEST, &error.to_string());
                write_frame(&mut writer, &response).await?;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let response = match serde_json::from_slice::<Value>(&line) {
            Ok(request) => server.handle(request).await,
            Err(_) => Some(error_response(
                Value::Null,
                PARSE_ERROR,
                "parse error: expected one JSON object",
            )),
        };
        let Some(response) = response else {
            continue;
        };
        write_frame(&mut writer, &response).await?;
    }
    Ok(())
}

/// Reads one newline-terminated request without letting a single line grow
/// past `MAX_LINE_BYTES`. An overlong line is an error, not a truncated parse.
async fn read_line_bounded<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            return Ok(Some(line));
        }
        if let Some(index) = available.iter().position(|byte| *byte == b'\n') {
            if line.len() + index + 1 > MAX_LINE_BYTES {
                return Err(too_long());
            }
            line.extend_from_slice(&available[..=index]);
            reader.consume(index + 1);
            return Ok(Some(line));
        }
        if line.len() + available.len() > MAX_LINE_BYTES {
            return Err(too_long());
        }
        line.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
}

fn too_long() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("request line exceeds {MAX_LINE_BYTES} bytes"),
    )
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, response: &Value) -> io::Result<()> {
    let mut frame = serde_json::to_string(response).unwrap_or_default();
    frame.push('\n');
    writer.write_all(frame.as_bytes()).await?;
    writer.flush().await
}

impl Server {
    /// Returns `None` for a notification, which by JSON-RPC gets no reply.
    async fn handle(&self, request: Value) -> Option<Value> {
        let Some(object) = request.as_object() else {
            return Some(error_response(
                Value::Null,
                INVALID_REQUEST,
                "invalid request: expected a JSON object",
            ));
        };
        let id = object.get("id").cloned();
        let method = object.get("method").and_then(Value::as_str);
        let Some(method) = method else {
            return id.map(|id| error_response(id, INVALID_REQUEST, "invalid request: no method"));
        };
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(initialize_result(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": [tool_definition()] })),
            "tools/call" => self.call_tool(&params).await,
            other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
        };
        let id = id?;
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => error_response(id, code, &message),
        })
    }

    async fn call_tool(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        if name != TOOL_NAME {
            return Err((INVALID_PARAMS, format!("unknown tool: {name}")));
        }
        let arguments = params
            .get("arguments")
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}));
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // A non-string message is ignored, like the plugin's `typeof` guard.
        let message = arguments
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string);
        let outcome = send_file(
            &self.publisher,
            &self.roots,
            &self.project_root,
            &SendFileInput { path, message },
        )
        .await;
        // A refusal is an answer, not a protocol failure: the tool never throws.
        Ok(json!({
            "content": [{ "type": "text", "text": outcome.content() }],
            "isError": false,
        }))
    }
}

fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let version = if PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        FALLBACK_VERSION
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "ntfy-relay",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn tool_definition() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": TOOL_DESCRIPTION,
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path, absolute or relative to the project directory.",
                },
                "message": {
                    "type": "string",
                    "description": "Optional short message shown with the file.",
                },
            },
            "required": ["path"],
            "additionalProperties": false,
        },
    })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_echoes_a_known_revision_and_falls_back_otherwise() {
        let known = initialize_result(&json!({ "protocolVersion": "2025-11-25" }));
        assert_eq!(known["protocolVersion"], "2025-11-25");
        let future = initialize_result(&json!({ "protocolVersion": "2027-01-01" }));
        assert_eq!(future["protocolVersion"], FALLBACK_VERSION);
        assert!(future["capabilities"]["tools"].is_object());
    }

    #[test]
    fn the_tool_keeps_the_plugin_contract() {
        let tool = tool_definition();
        assert_eq!(tool["name"], TOOL_NAME);
        assert_eq!(tool["inputSchema"]["required"], json!(["path"]));
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert_eq!(tool["inputSchema"]["properties"]["path"]["type"], "string");
    }

    #[tokio::test]
    async fn a_request_line_at_the_cap_is_read_and_a_longer_one_fails() {
        let payload = vec![b'x'; MAX_LINE_BYTES];
        let mut reader = tokio::io::BufReader::new(&payload[..]);
        let line = read_line_bounded(&mut reader).await.unwrap().unwrap();
        assert_eq!(line.len(), MAX_LINE_BYTES);

        let payload = vec![b'x'; MAX_LINE_BYTES + 1];
        let mut reader = tokio::io::BufReader::new(&payload[..]);
        let error = read_line_bounded(&mut reader).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("request line exceeds"));
    }

    #[tokio::test]
    async fn the_last_line_without_a_newline_is_still_read() {
        let mut reader = tokio::io::BufReader::new(&b"{\"x\":1}"[..]);
        let line = read_line_bounded(&mut reader).await.unwrap().unwrap();
        assert_eq!(line, b"{\"x\":1}");
        assert!(read_line_bounded(&mut reader).await.unwrap().is_none());
    }
}
