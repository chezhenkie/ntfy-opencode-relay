//! Attachment fetch into the project inbox.
//!
//! Mirrors `downloadAttachment` and `sanitizeFilename` in
//! `ntfy-inbox-relay.mjs`: HTTPS on the first URL and every redirect hop, at
//! most three followed redirects, a declared and streamed size cap, and the
//! same inbox file naming rules.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::http::{transport_allowed, HttpError, HttpRequest, Transport};

/// Attachment downloads share one 60 s deadline across all redirect hops.
pub const ATTACHMENT_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum followed redirects; the fourth response is the final one.
pub const MAX_REDIRECTS: usize = 3;

const REDIRECT_STATUS: [u16; 5] = [301, 302, 303, 307, 308];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub url: String,
    pub name: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedAttachment {
    pub path: PathBuf,
    pub bytes: u64,
    pub name: String,
}

#[derive(Debug)]
pub enum AttachError {
    InvalidUrl,
    TransportRequired,
    Redirects,
    RedirectWithoutLocation,
    Status(u16),
    TooLarge { limit: u64 },
    DeclaredTooLarge { declared: u64, limit: u64 },
    SizeMismatch { declared: u64, actual: u64 },
    Timeout,
    Transport(HttpError),
    Io(std::io::Error),
    HostMismatch,
}

impl fmt::Display for AttachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl => f.write_str("attachment: invalid url"),
            Self::TransportRequired => {
                f.write_str("attachment: HTTPS required unless the host is loopback")
            }
            Self::HostMismatch => {
                f.write_str("attachment: host is not the ntfy server or the deployment host")
            }
            Self::Redirects => {
                write!(f, "attachment: more than {MAX_REDIRECTS} redirects")
            }
            Self::RedirectWithoutLocation => f.write_str("attachment: redirect without location"),
            Self::Status(status) => write!(f, "attachment: HTTP {status}"),
            Self::TooLarge { limit } => {
                write!(f, "attachment: exceeds {limit} bytes")
            }
            Self::DeclaredTooLarge { declared, limit } => {
                write!(f, "attachment: {declared} bytes exceeds {limit}")
            }
            Self::SizeMismatch { declared, actual } => {
                write!(
                    f,
                    "attachment: size mismatch, expected {declared}, got {actual}"
                )
            }
            Self::Timeout => f.write_str("attachment: timed out"),
            Self::Transport(error) => write!(f, "attachment: {error}"),
            Self::Io(error) => write!(f, "attachment: {error}"),
        }
    }
}

impl std::error::Error for AttachError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AttachError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Downloads one attachment and writes it as `<epoch>-<name>` under
/// `dest_dir`. Every hop must use a host in `allowed_hosts`: the ntfy server
/// host plus, when a deployment config is present, the deployment base URL
/// host. A hop on the deployment host is fetched through `server_base`, the
/// configured ntfy server, so the relay never opens a plaintext connection to
/// the mesh or LAN address.
pub async fn fetch_attachment(
    transport: &dyn Transport,
    attachment: &Attachment,
    allowed_hosts: &[String],
    server_base: &str,
    dest_dir: &Path,
    max_bytes: u64,
    now_ms: i64,
) -> Result<SavedAttachment, AttachError> {
    if let Some(declared) = attachment.size {
        if declared > max_bytes {
            return Err(AttachError::DeclaredTooLarge {
                declared,
                limit: max_bytes,
            });
        }
    }
    let download = download(
        transport,
        attachment,
        allowed_hosts,
        server_base,
        dest_dir,
        max_bytes,
        now_ms,
    );
    tokio::time::timeout(ATTACHMENT_TIMEOUT, download)
        .await
        .map_err(|_| AttachError::Timeout)?
}

async fn download(
    transport: &dyn Transport,
    attachment: &Attachment,
    allowed_hosts: &[String],
    server_base: &str,
    dest_dir: &Path,
    max_bytes: u64,
    now_ms: i64,
) -> Result<SavedAttachment, AttachError> {
    let mut url = require_host(&attachment.url, allowed_hosts)?;
    let mut hops = 0;
    let body = loop {
        let target = local_target(&url, server_base)?;
        let request = HttpRequest::new(crate::http::Method::Get, target.as_str())
            .max_response_bytes(max_bytes);
        let response = transport.send(request).await.map_err(|error| match error {
            HttpError::TooLarge { limit } => AttachError::TooLarge { limit },
            other => AttachError::Transport(other),
        })?;
        if REDIRECT_STATUS.contains(&response.status) {
            if hops >= MAX_REDIRECTS {
                return Err(AttachError::Redirects);
            }
            let location = response
                .header("location")
                .ok_or(AttachError::RedirectWithoutLocation)?;
            let next = url.join(location).map_err(|_| AttachError::InvalidUrl)?;
            url = require_host(next.as_str(), allowed_hosts)?;
            hops += 1;
            continue;
        }
        if !(200..300).contains(&response.status) {
            return Err(AttachError::Status(response.status));
        }
        break response.body;
    };

    let total = body.len() as u64;
    if total > max_bytes {
        return Err(AttachError::TooLarge { limit: max_bytes });
    }
    if let Some(declared) = attachment.size {
        if declared > 0 && declared != total {
            return Err(AttachError::SizeMismatch {
                declared,
                actual: total,
            });
        }
    }

    let fallback = format!("file-{now_ms}");
    let name = sanitize_filename(attachment.name.as_deref().unwrap_or(""), &fallback);
    let target = unique_target(dest_dir, &name, now_ms);
    tokio::fs::create_dir_all(dest_dir).await?;
    if let Err(error) = tokio::fs::write(&target, &body).await {
        let _ = tokio::fs::remove_file(&target).await;
        return Err(AttachError::Io(error));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o600);
        if let Err(error) = tokio::fs::set_permissions(&target, permissions).await {
            let _ = tokio::fs::remove_file(&target).await;
            return Err(AttachError::Io(error));
        }
    }
    Ok(SavedAttachment {
        path: target,
        bytes: total,
        name,
    })
}

/// `<seconds>-<name>`, then the millisecond name, then a counter. The write
/// must never truncate a file another process or a retry left in the inbox.
fn unique_target(dest_dir: &Path, name: &str, now_ms: i64) -> PathBuf {
    let seconds = now_ms.div_euclid(1000);
    let first = dest_dir.join(format!("{seconds}-{name}"));
    if !first.exists() {
        return first;
    }
    let second = dest_dir.join(format!("{now_ms}-{name}"));
    if !second.exists() {
        return second;
    }
    let mut counter = 1u64;
    loop {
        let candidate = dest_dir.join(format!("{now_ms}-{counter}-{name}"));
        if !candidate.exists() {
            return candidate;
        }
        counter += 1;
    }
}

fn require_host(value: &str, allowed_hosts: &[String]) -> Result<url::Url, AttachError> {
    let url = url::Url::parse(value).map_err(|_| AttachError::InvalidUrl)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AttachError::TransportRequired);
    }
    let host = url.host_str().ok_or(AttachError::InvalidUrl)?;
    if !allowed_hosts
        .iter()
        .any(|allowed| host.eq_ignore_ascii_case(allowed))
    {
        return Err(AttachError::HostMismatch);
    }
    Ok(url)
}

/// The request URL for one hop. A hop already on the configured server host is
/// fetched as given; a hop on the deployment host reuses the server base and
/// the attachment path. The rewritten target still has to pass the transport
/// rule, which is why a non-loopback HTTP base is never contacted.
fn local_target(original: &url::Url, server_base: &str) -> Result<url::Url, AttachError> {
    let base = url::Url::parse(server_base).map_err(|_| AttachError::InvalidUrl)?;
    let same_host = original
        .host_str()
        .zip(base.host_str())
        .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b));
    let target = if same_host {
        original.clone()
    } else {
        let mut target = base;
        target.set_path(original.path());
        target.set_query(original.query());
        target
    };
    if !transport_allowed(&target) {
        return Err(AttachError::TransportRequired);
    }
    Ok(target)
}

/// Keeps only `[A-Za-z0-9._-]` in the last path segment, strips leading dots,
/// cuts at 120 characters, and prefixes DOS device names with `_`.
pub fn sanitize_filename(name: &str, fallback: &str) -> String {
    let normalized = name.replace('\\', "/");
    let last = normalized.rsplit('/').next().unwrap_or("");
    let filtered: String = last
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    let trimmed = filtered.trim_start_matches('.');
    let cut: String = trimmed.chars().take(120).collect();
    let mut base = if cut.is_empty() || cut == "." || cut == ".." {
        fallback.to_string()
    } else {
        cut
    };
    if is_reserved(&base) {
        base.insert(0, '_');
    }
    base
}

fn is_reserved(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.len() < 3 {
        return false;
    }
    let prefix = &lower[..3];
    if matches!(prefix, "con" | "prn" | "aux" | "nul") {
        return lower.len() == 3 || lower.as_bytes()[3] == b'.';
    }
    if lower.len() >= 4 {
        let digit = lower.as_bytes()[3];
        if (prefix == "com" || prefix == "lpt") && (b'1'..=b'9').contains(&digit) {
            return lower.len() == 4 || lower.as_bytes()[4] == b'.';
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_matches_relay_cases() {
        assert_eq!(sanitize_filename("../../a b.txt", "file"), "ab.txt");
        assert_eq!(
            sanitize_filename("R\u{e9}sum\u{e9}.pdf", "file"),
            "Rsum.pdf"
        );
        assert_eq!(sanitize_filename("", "file"), "file");
        assert_eq!(sanitize_filename("..", "file"), "file");
        assert_eq!(sanitize_filename("CON.txt", "file"), "_CON.txt");
        assert_eq!(sanitize_filename("lpt9", "file"), "_lpt9");
        assert_eq!(sanitize_filename("console.txt", "file"), "console.txt");
    }
}
