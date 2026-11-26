//! Auto-attach and the explicit file send.
//!
//! Mirrors `phone-files.js`: the terminal scan is the deterministic source for
//! files created during a turn, because the server's recursive watcher is not
//! reliable for nested directories. Only files whose birthtime falls inside the
//! turn window are candidates, never modified source files. The explicit send
//! backs the future MCP `send_file` tool: project-confined, 15 MB cap, and the
//! `oc-out,oc-manual` tags.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use ntfy_relay_core::link_safe::link_safe;
use ntfy_relay_core::topic::topic_for_project;
use ntfy_relay_ntfy::header::Priority;
use ntfy_relay_ntfy::publish::Publisher;

use crate::log::Log;
use crate::publish::{send, OutboundFile, OutboundMessage};
use crate::scope::{canonical, is_inside, normalize_for_compare, within_roots};

/// Per-file auto-attach cap, matching the spec.
pub const AUTO_ATTACH_MAX_FILE_BYTES: u64 = 10_485_760;
/// Per-turn auto-attach totals, matching the spec.
pub const AUTO_ATTACH_MAX_FILES: usize = 8;
pub const AUTO_ATTACH_MAX_TOTAL_BYTES: u64 = 31_457_280;
/// Explicit tool cap.
pub const TOOL_MAX_FILE_BYTES: u64 = 15_728_640;
/// Upper bound on directory entries visited by the per-turn new-file scan.
pub const SCAN_MAX_ENTRIES: usize = 20_000;

pub const EXCLUDED_SEGMENTS: &[&str] = &[
    ".git",
    "node_modules",
    ".cache",
    ".ntfy-inbox",
    ".opencode",
    "logs",
    "dist",
    "build",
    "target",
    "venv",
    ".venv",
    "__pycache__",
    "temp",
    "resources",
    "bin",
    ".scratch",
];

const CONTENT_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
    ("bmp", "image/bmp"),
    ("tiff", "image/tiff"),
    ("svg", "image/svg+xml"),
    ("avif", "image/avif"),
    ("heic", "image/heic"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("ogg", "audio/ogg"),
    ("flac", "audio/flac"),
    ("m4a", "audio/mp4"),
    ("aac", "audio/aac"),
    ("opus", "audio/opus"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("mov", "video/quicktime"),
    ("mkv", "video/x-matroska"),
    ("pdf", "application/pdf"),
    ("txt", "text/plain; charset=utf-8"),
    ("md", "text/markdown; charset=utf-8"),
    ("csv", "text/csv; charset=utf-8"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("epub", "application/epub+zip"),
    ("zip", "application/zip"),
    ("7z", "application/x-7z-compressed"),
    ("tar", "application/x-tar"),
    ("gz", "application/gzip"),
];

#[derive(Debug, Clone)]
pub struct AutoAttachSettings {
    pub enabled: bool,
    pub extensions: Vec<String>,
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub scan_max_entries: usize,
}

impl Default for AutoAttachSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            extensions: ntfy_relay_core::config::DEFAULT_AUTO_ATTACH_EXTENSIONS
                .iter()
                .map(|value| value.to_string())
                .collect(),
            max_files: AUTO_ATTACH_MAX_FILES,
            max_file_bytes: AUTO_ATTACH_MAX_FILE_BYTES,
            max_total_bytes: AUTO_ATTACH_MAX_TOTAL_BYTES,
            scan_max_entries: SCAN_MAX_ENTRIES,
        }
    }
}

#[derive(Debug)]
pub struct Inspected {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub modified_ms: i64,
    pub created_ms: i64,
    /// The opened file. Reads use this handle, never the path again, so a
    /// symlink swapped in after inspect cannot redirect the read.
    handle: Option<std::fs::File>,
}

impl Inspected {
    /// Reads at most `max_bytes + 1` from the inspected handle. The caller
    /// compares the length with the cap, so a file that grew after the stat
    /// stays bounded.
    pub async fn read(&mut self, max_bytes: u64) -> std::io::Result<Vec<u8>> {
        let handle = self
            .handle
            .take()
            .ok_or_else(|| std::io::Error::other("file was already read"))?;
        let capacity = self.size.min(max_bytes) as usize;
        tokio::task::spawn_blocking(move || {
            let mut data = Vec::with_capacity(capacity);
            let mut limited = handle.take(max_bytes.saturating_add(1));
            limited.read_to_end(&mut data)?;
            Ok::<Vec<u8>, std::io::Error>(data)
        })
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: &'static str,
    /// The file exists and is inside the project but exceeds a byte cap.
    pub cap: bool,
}

impl Refusal {
    fn new(reason: &'static str) -> Self {
        Self { reason, cap: false }
    }

    fn cap(reason: &'static str) -> Self {
        Self { reason, cap: true }
    }
}

/// Resolves one path against the project canonical and applies the containment,
/// allowlist, exclusion, and size rules. The file is opened once, the handle is
/// stat'ed, and the same handle is read later; the path is only used to open.
pub fn inspect(
    raw: &str,
    base: &Path,
    root: &Path,
    allowlist: Option<&[String]>,
    exclude_segments: bool,
    max_bytes: u64,
) -> Result<Inspected, Refusal> {
    if raw.trim().is_empty() {
        return Err(Refusal::new("path is missing"));
    }
    let raw_path = Path::new(raw);
    let absolute = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        base.join(raw_path)
    };
    let path = canonical(&absolute).ok_or_else(|| Refusal::new("file does not exist"))?;
    let root = canonical(root).unwrap_or_else(|| root.to_path_buf());
    if !is_inside(&root, &path) {
        return Err(Refusal::new("path is outside the project"));
    }
    let handle = std::fs::File::open(&path).map_err(|_| Refusal::new("file does not exist"))?;
    let metadata = handle
        .metadata()
        .map_err(|_| Refusal::new("file does not exist"))?;
    if !metadata.is_file() {
        return Err(Refusal::new("path is not a regular file"));
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(allowlist) = allowlist {
        let extension = extension_of(&name);
        if !allowlist.contains(&extension) {
            return Err(Refusal::new("extension is not allowed"));
        }
    }
    if exclude_segments {
        let relative = path.strip_prefix(&root).unwrap_or(&path);
        if relative.components().any(|component| {
            let segment = component.as_os_str().to_string_lossy().to_ascii_lowercase();
            EXCLUDED_SEGMENTS.contains(&segment.as_str())
        }) {
            return Err(Refusal::new("path is in an excluded directory"));
        }
    }
    if metadata.len() > max_bytes {
        return Err(Refusal::cap("file is too large"));
    }
    Ok(Inspected {
        name,
        size: metadata.len(),
        modified_ms: timestamp_ms(metadata.modified()),
        created_ms: timestamp_ms(metadata.created()),
        path,
        handle: Some(handle),
    })
}

fn extension_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn timestamp_ms(value: Result<std::time::SystemTime, std::io::Error>) -> i64 {
    value
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// A birthtime of zero means the filesystem does not report creation time; the
/// plugin skipped those files too, so an unmodified file can never be attached
/// by mistake.
pub fn is_new_file(created_ms: i64, cutoff: i64) -> bool {
    created_ms > 0 && created_ms >= cutoff
}

pub fn content_type_for(name: &str) -> &'static str {
    let extension = extension_of(name);
    CONTENT_TYPES
        .iter()
        .find(|(key, _)| *key == extension)
        .map(|(_, value)| *value)
        .unwrap_or("application/octet-stream")
}

/// Walks the project for files created at or after `started_at - 1000 ms`.
/// Directory reads and stats are synchronous; callers run it on a blocking
/// task. Symlinks are skipped and excluded directories are not entered.
pub fn scan_new_files(root: &Path, started_at: i64, settings: &AutoAttachSettings) -> Vec<PathBuf> {
    let cutoff = started_at - 1000;
    let root = canonical(root).unwrap_or_else(|| root.to_path_buf());
    let mut found = Vec::new();
    let mut visited = 0usize;
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        if visited >= settings.scan_max_entries {
            break;
        }
        let entries = match std::fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > settings.scan_max_entries {
                break;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if !EXCLUDED_SEGMENTS.contains(&name.as_str()) {
                    stack.push(path);
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !settings
                .extensions
                .iter()
                .any(|extension| *extension == extension_of(&name))
            {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.len() > settings.max_file_bytes {
                continue;
            }
            let created = timestamp_ms(metadata.created());
            if is_new_file(created, cutoff) {
                found.push(path);
            }
        }
    }
    found
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutoAttachStats {
    pub published: usize,
    pub skipped: usize,
    pub bytes: u64,
}

/// Filters and publishes the files added during one turn. One message per
/// file, plus one summary line when a cap skipped something.
#[allow(clippy::too_many_arguments)]
pub async fn publish_auto_attached(
    publisher: &Publisher,
    topic: &str,
    root: &Path,
    collected: &[String],
    started_at: i64,
    settings: &AutoAttachSettings,
    log: &dyn Log,
) -> AutoAttachStats {
    let mut stats = AutoAttachStats::default();
    if !settings.enabled {
        return stats;
    }
    let scan_root = root.to_path_buf();
    let scan_settings = settings.clone();
    let scanned =
        tokio::task::spawn_blocking(move || scan_new_files(&scan_root, started_at, &scan_settings))
            .await
            .unwrap_or_default();
    let mut all_paths: Vec<String> = collected.to_vec();
    all_paths.extend(
        scanned
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned()),
    );
    if all_paths.is_empty() {
        return stats;
    }

    let cutoff = started_at - 1000;
    let mut seen = std::collections::HashSet::new();
    let mut candidates = Vec::new();
    for raw in all_paths {
        let inspected = match inspect(
            &raw,
            root,
            root,
            Some(&settings.extensions),
            true,
            settings.max_file_bytes,
        ) {
            Ok(inspected) => inspected,
            Err(refusal) => {
                if refusal.cap {
                    stats.skipped += 1;
                }
                continue;
            }
        };
        if inspected.created_ms > 0 && inspected.created_ms < cutoff {
            continue;
        }
        let key = normalize_for_compare(&inspected.path);
        if !seen.insert(key) {
            continue;
        }
        candidates.push(inspected);
    }
    candidates.sort_by(|a, b| {
        a.modified_ms
            .cmp(&b.modified_ms)
            .then_with(|| a.path.cmp(&b.path))
    });

    let mut selected = Vec::new();
    let mut total = 0u64;
    for candidate in candidates {
        if selected.len() >= settings.max_files || total + candidate.size > settings.max_total_bytes
        {
            stats.skipped += 1;
            continue;
        }
        total += candidate.size;
        selected.push(candidate);
    }

    for mut candidate in selected {
        let data = match candidate.read(settings.max_file_bytes).await {
            Ok(data) => data,
            Err(_) => {
                log.warn("auto-attached file could not be read");
                continue;
            }
        };
        if data.len() as u64 > settings.max_file_bytes {
            stats.skipped += 1;
            continue;
        }
        let file = OutboundFile {
            filename: candidate.name.clone(),
            content_type: content_type_for(&candidate.name).to_string(),
            data: Bytes::from(data),
        };
        let message = OutboundMessage {
            kind: "session.file",
            topic: topic.to_string(),
            title: format!("{topic} - {}", link_safe(&candidate.name)),
            message: format!("{} bytes", candidate.size),
            tags: vec!["oc-out", "oc-attach"],
            priority: Priority::Low,
            file: Some(file),
        };
        match send(publisher, &message).await {
            Ok(()) => {
                stats.published += 1;
                stats.bytes += candidate.size;
            }
            Err(error) => log.warn(&format!("auto-attached file publish failed: {error}")),
        }
    }
    if stats.skipped > 0 {
        let message = OutboundMessage {
            kind: "session.file",
            topic: topic.to_string(),
            title: topic.to_string(),
            message: format!("{} file(s) were not attached (turn caps).", stats.skipped),
            tags: vec!["oc-out", "oc-attach"],
            priority: Priority::Min,
            file: None,
        };
        if let Err(error) = send(publisher, &message).await {
            log.warn(&format!("turn cap summary publish failed: {error}"));
        }
    }
    stats
}

#[derive(Debug, Clone, Default)]
pub struct SendFileInput {
    pub path: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendFileOutcome {
    Sent { name: String, bytes: u64 },
    Refused(String),
}

impl SendFileOutcome {
    pub fn content(&self) -> String {
        match self {
            Self::Sent { name, bytes } => {
                format!("Sent {name} ({bytes} bytes) to the phone.")
            }
            Self::Refused(reason) => format!("Not sent: {reason}."),
        }
    }
}

/// The library call behind the future MCP tool. The project root must be one of
/// the configured roots; the path must stay inside it.
pub async fn send_file(
    publisher: &Publisher,
    roots: &[PathBuf],
    project_root: &Path,
    input: &SendFileInput,
) -> SendFileOutcome {
    if !within_roots(project_root, roots) {
        return SendFileOutcome::Refused("project is outside the configured roots".to_string());
    }
    let project_text = project_root.to_string_lossy().into_owned();
    let Some(topic) = topic_for_project(&project_text) else {
        return SendFileOutcome::Refused("project folder name is not a valid topic".to_string());
    };
    let mut inspected = match inspect(
        &input.path,
        project_root,
        project_root,
        None,
        false,
        TOOL_MAX_FILE_BYTES,
    ) {
        Ok(inspected) => inspected,
        Err(refusal) => return SendFileOutcome::Refused(refusal.reason.to_string()),
    };
    let data = match inspected.read(TOOL_MAX_FILE_BYTES).await {
        Ok(data) => data,
        Err(_) => return SendFileOutcome::Refused("file could not be read".to_string()),
    };
    if data.len() as u64 > TOOL_MAX_FILE_BYTES {
        return SendFileOutcome::Refused("file is too large".to_string());
    }
    let message = match input.message.as_deref() {
        Some(value) if !value.trim().is_empty() => link_safe(value),
        _ => String::new(),
    };
    let file = OutboundFile {
        filename: inspected.name.clone(),
        content_type: content_type_for(&inspected.name).to_string(),
        data: Bytes::from(data),
    };
    let outbound = OutboundMessage {
        kind: "session.file",
        topic: topic.to_string(),
        title: format!("{topic} - {}", link_safe(&inspected.name)),
        message,
        tags: vec!["oc-out", "oc-manual"],
        priority: Priority::Default,
        file: Some(file),
    };
    match send(publisher, &outbound).await {
        Ok(()) => SendFileOutcome::Sent {
            name: inspected.name,
            bytes: inspected.size,
        },
        Err(_) => SendFileOutcome::Refused("ntfy publish failed".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ntfy_relay_ntfy::http::{HttpError, HttpRequest, HttpResponse};
    use ntfy_relay_ntfy::publish::NtfyServer;
    use std::sync::{Arc, Mutex};

    fn temp_root(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("outbound-file-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[derive(Default)]
    struct Recorder {
        requests: Mutex<Vec<HttpRequest>>,
    }

    #[async_trait]
    impl ntfy_relay_ntfy::http::Transport for Recorder {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
            self.requests.lock().unwrap().push(request);
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: Bytes::new(),
            })
        }
    }

    fn publisher(transport: Arc<Recorder>) -> Publisher {
        Publisher::new(
            NtfyServer::parse("https://ntfy.test").unwrap(),
            Some("token-1234567890".to_string()),
            transport,
        )
    }

    fn settings() -> AutoAttachSettings {
        AutoAttachSettings {
            scan_max_entries: 1000,
            ..AutoAttachSettings::default()
        }
    }

    #[test]
    fn inspect_enforces_project_scope_and_caps() {
        let root = temp_root("inspect");
        let project = root.join("proj");
        std::fs::create_dir_all(project.join("sub")).unwrap();
        let inside = project.join("sub/note.md");
        std::fs::write(&inside, b"hello").unwrap();
        let outside = root.join("outside.md");
        std::fs::write(&outside, b"x").unwrap();

        let found = inspect(
            "sub/note.md",
            &project,
            &project,
            Some(&["md".to_string()]),
            true,
            10,
        )
        .unwrap();
        assert_eq!(found.name, "note.md");
        assert_eq!(found.size, 5);

        assert_eq!(
            inspect("../outside.md", &project, &project, None, true, 10)
                .unwrap_err()
                .reason,
            "path is outside the project"
        );
        assert_eq!(
            inspect(
                "sub/note.md",
                &project,
                &project,
                Some(&["png".to_string()]),
                true,
                10
            )
            .unwrap_err()
            .reason,
            "extension is not allowed"
        );
        assert_eq!(
            inspect("sub/note.md", &project, &project, None, true, 2)
                .unwrap_err()
                .reason,
            "file is too large"
        );
        assert_eq!(
            inspect("missing.md", &project, &project, None, true, 10)
                .unwrap_err()
                .reason,
            "file does not exist"
        );
        std::fs::create_dir_all(project.join("node_modules")).unwrap();
        std::fs::write(project.join("node_modules/x.md"), b"x").unwrap();
        assert_eq!(
            inspect("node_modules/x.md", &project, &project, None, true, 10)
                .unwrap_err()
                .reason,
            "path is in an excluded directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_outside_fifo_is_refused_without_an_open() {
        let root = temp_root("fifo");
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let fifo = root.join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !matches!(made, Ok(status) if status.success()) {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let threaded = project.clone();
        std::thread::spawn(move || {
            let outcome = inspect("../pipe", &threaded, &threaded, None, true, 1024)
                .map(|_| ())
                .map_err(|refusal| refusal.reason);
            let _ = tx.send(outcome);
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Err(reason)) => assert_eq!(reason, "path is outside the project"),
            Ok(Ok(())) => panic!("an outside fifo was accepted"),
            Err(_) => panic!("inspect opened the fifo before the containment check"),
        }
    }

    #[test]
    fn the_new_file_rule_requires_a_reported_birthtime() {
        assert!(is_new_file(2_000, 1_000));
        assert!(!is_new_file(500, 1_000));
        assert!(!is_new_file(0, 1_000));
    }

    #[test]
    fn the_scan_keeps_only_files_created_inside_the_window() {
        let root = temp_root("scan");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("new.png"), b"x").unwrap();
        std::fs::write(root.join("sub/also.md"), b"x").unwrap();
        std::fs::write(root.join("skip.exe"), b"x").unwrap();
        let probe = std::fs::metadata(root.join("new.png"))
            .map(|metadata| timestamp_ms(metadata.created()))
            .unwrap_or(0);
        if probe == 0 {
            // 9P and some Linux filesystems report no birthtime; the scan is
            // inert there by design, so only the rule above is testable.
            return;
        }
        let now = timestamp_ms(Ok(std::time::SystemTime::now()));
        let found = scan_new_files(&root, now - 60_000, &settings());
        let names: Vec<String> = found
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"new.png".to_string()));
        assert!(names.contains(&"also.md".to_string()));
        assert!(!names.contains(&"skip.exe".to_string()));

        // A window that starts in the future excludes everything already born.
        let none = scan_new_files(&root, now + 3_600_000, &settings());
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn auto_attach_publishes_one_message_per_file() {
        let root = temp_root("auto");
        std::fs::write(root.join("one.png"), b"12345").unwrap();
        std::fs::write(root.join("two.md"), b"abc").unwrap();
        let transport = Arc::new(Recorder::default());
        let publisher = publisher(transport.clone());
        let now = timestamp_ms(Ok(std::time::SystemTime::now()));
        let stats = publish_auto_attached(
            &publisher,
            "proj",
            &root,
            &["one.png".to_string(), "two.md".to_string()],
            now - 60_000,
            &settings(),
            &crate::log::NoopLog,
        )
        .await;
        assert_eq!(stats.published, 2);
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].url.contains("?title=proj"));
        assert!(requests[0].url.contains("tags=oc-out%2Coc-attach"));
        assert!(requests[0].url.contains("priority=2"));
    }

    #[tokio::test]
    async fn a_read_uses_the_inspected_handle() {
        let root = temp_root("handle");
        let path = root.join("note.md");
        std::fs::write(&path, b"original").unwrap();
        let mut inspected = inspect("note.md", &root, &root, None, false, 1024).unwrap();
        // Swap the path after inspect; the handle must still read the original.
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replaced").unwrap();
        let data = inspected.read(1024).await.unwrap();
        assert_eq!(data, b"original");
    }

    #[tokio::test]
    async fn a_file_that_grew_is_read_only_up_to_the_cap() {
        let root = temp_root("growth");
        let path = root.join("grow.md");
        std::fs::write(&path, b"four").unwrap();
        let mut inspected = inspect("grow.md", &root, &root, None, false, 8).unwrap();
        let mut handle = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut handle, &[b'x'; 100]).unwrap();
        drop(handle);
        let data = inspected.read(8).await.unwrap();
        assert_eq!(data.len(), 9, "one byte over the cap proves the truncation");
        assert!(data.len() as u64 > 8);
    }

    #[tokio::test]
    async fn send_file_is_project_confined_and_reports_the_plugin_text() {
        let root = temp_root("send");
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("note.md"), b"hello").unwrap();
        let transport = Arc::new(Recorder::default());
        let publisher = publisher(transport.clone());

        let sent = send_file(
            &publisher,
            std::slice::from_ref(&root),
            &project,
            &SendFileInput {
                path: "note.md".to_string(),
                message: Some("see readme.md".to_string()),
            },
        )
        .await;
        assert_eq!(sent.content(), "Sent note.md (5 bytes) to the phone.");
        {
            let requests = transport.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].url.starts_with("https://ntfy.test/proj?"));
            assert!(requests[0].url.contains("tags=oc-out%2Coc-manual"));
            assert!(requests[0].url.contains("readme%5B.%5Dmd"));
            assert_eq!(
                requests[0].header_value("content-type"),
                Some("text/markdown; charset=utf-8")
            );
        }

        let outside = send_file(
            &publisher,
            std::slice::from_ref(&project),
            &root,
            &SendFileInput {
                path: "proj/note.md".to_string(),
                message: None,
            },
        )
        .await;
        assert_eq!(
            outside.content(),
            "Not sent: project is outside the configured roots."
        );
    }
}
