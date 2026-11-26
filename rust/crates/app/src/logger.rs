//! File logger with one rotated copy.
//!
//! Mirrors the Node `Logger`: ISO timestamp, level, message, append per line,
//! rotate at 5 MB into `<file>.1`, and never let a write failure stop the
//! relay. Echo is off in the deployed daemon because it runs hidden.
//!
//! One writer thread owns an open handle. Callers format and enqueue a line;
//! the runtime thread never opens, appends, or rotates, so a slow disk or a
//! rotate cannot stall the select loop. The handle is reopened after a rotate
//! or a write failure. The thread ends, after draining, when the last logger
//! clone drops; `flush` waits for the queue, and the drop waits at most 2 s.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ntfy_relay_core::config::LogLevel;

pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// A hung writer must not hold process exit past this.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

enum Command {
    Line(String),
    Flush(mpsc::Sender<()>),
}

struct Inner {
    file: Option<PathBuf>,
    threshold: u8,
    echo: bool,
    sender: Mutex<Option<mpsc::Sender<Command>>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        drop(
            self.sender
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take(),
        );
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            join_with_deadline(worker, JOIN_TIMEOUT);
        }
    }
}

/// Joins the writer on a helper thread so a hung write cannot hold exit. A
/// writer that misses the deadline is left detached.
fn join_with_deadline(worker: thread::JoinHandle<()>, deadline: Duration) {
    let (done, finished) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("log-join".to_string())
        .spawn(move || {
            let _ = worker.join();
            let _ = done.send(());
        });
    let _ = finished.recv_timeout(deadline);
}

#[derive(Clone)]
pub struct Logger {
    inner: Arc<Inner>,
}

impl Logger {
    pub fn new(file: Option<PathBuf>, level: LogLevel, echo: bool, max_bytes: u64) -> Self {
        let (sender, receiver) = mpsc::channel::<Command>();
        let worker_file = file.clone();
        let worker = thread::Builder::new()
            .name("log-writer".to_string())
            .spawn(move || writer_loop(worker_file, max_bytes, receiver))
            .expect("log writer thread");
        Self {
            inner: Arc::new(Inner {
                file,
                threshold: level.as_rank(),
                echo,
                sender: Mutex::new(Some(sender)),
                worker: Mutex::new(Some(worker)),
            }),
        }
    }

    /// Creates the log directory and tightens an existing file on Unix.
    pub fn init(&self) -> io::Result<()> {
        let Some(file) = &self.inner.file else {
            return Ok(());
        };
        if let Some(parent) = file.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        #[cfg(unix)]
        if file.exists() {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(file, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Blocks until every queued line has been written.
    pub fn flush(&self) {
        let (done, finished) = mpsc::channel();
        let sender = self
            .inner
            .sender
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(sender) = sender.as_ref() {
            if sender.send(Command::Flush(done)).is_ok() {
                let _ = finished.recv();
            }
        }
    }

    pub fn debug(&self, message: &str) {
        self.log("DEBUG", LogLevel::Debug.as_rank(), message);
    }

    pub fn info(&self, message: &str) {
        self.log("INFO", LogLevel::Info.as_rank(), message);
    }

    pub fn warn(&self, message: &str) {
        self.log("WARN", LogLevel::Warn.as_rank(), message);
    }

    pub fn error(&self, message: &str) {
        self.log("ERROR", LogLevel::Error.as_rank(), message);
    }

    fn log(&self, level: &str, rank: u8, message: &str) {
        if rank < self.inner.threshold {
            return;
        }
        let line = format!(
            "{} {level} {message}",
            format_timestamp(crate::util::now_ms())
        );
        if self.inner.echo {
            if rank >= LogLevel::Warn.as_rank() {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
        if self.inner.file.is_none() {
            return;
        }
        let sender = self
            .inner
            .sender
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(sender) = sender.as_ref() {
            let _ = sender.send(Command::Line(line));
        }
    }
}

struct Writer {
    file: PathBuf,
    max_bytes: u64,
    /// The open handle and the bytes already in the file.
    handle: Option<(fs::File, u64)>,
}

impl Writer {
    fn new(file: PathBuf, max_bytes: u64) -> Self {
        Self {
            file,
            max_bytes,
            handle: None,
        }
    }

    fn write(&mut self, line: &str) {
        if self.handle.is_none() {
            self.open();
        }
        let over = self
            .handle
            .as_ref()
            .map(|(_, size)| *size >= self.max_bytes)
            .unwrap_or(false);
        if over {
            self.rotate();
        }
        let Some((handle, size)) = self.handle.as_mut() else {
            return;
        };
        let written = format!("{line}\n");
        match handle.write_all(written.as_bytes()) {
            Ok(()) => *size += written.len() as u64,
            Err(error) => {
                eprintln!("log write failed: {error}");
                // Reopen on the next line; a failed handle stays unusable.
                self.handle = None;
            }
        }
    }

    fn open(&mut self) {
        if let Some(parent) = self.file.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }
        let size = fs::metadata(&self.file).map(|meta| meta.len()).unwrap_or(0);
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&self.file) {
            Ok(handle) => self.handle = Some((handle, size)),
            Err(error) => eprintln!("log write failed: {error}"),
        }
    }

    fn rotate(&mut self) {
        self.handle = None;
        rotate(&self.file);
        self.open();
    }

    fn flush(&mut self) {
        if let Some((handle, _)) = self.handle.as_mut() {
            let _ = handle.flush();
        }
    }
}

fn writer_loop(file: Option<PathBuf>, max_bytes: u64, receiver: Receiver<Command>) {
    let Some(file) = file else {
        // No file: drain so a flush still answers.
        for command in receiver {
            if let Command::Flush(done) = command {
                let _ = done.send(());
            }
        }
        return;
    };
    let mut writer = Writer::new(file, max_bytes);
    for command in receiver {
        match command {
            Command::Line(line) => writer.write(&line),
            Command::Flush(done) => {
                writer.flush();
                let _ = done.send(());
            }
        }
    }
    writer.flush();
}

impl ntfy_relay_outbound::log::Log for Logger {
    fn debug(&self, message: &str) {
        Logger::debug(self, message);
    }

    fn info(&self, message: &str) {
        Logger::info(self, message);
    }

    fn warn(&self, message: &str) {
        Logger::warn(self, message);
    }

    fn error(&self, message: &str) {
        Logger::error(self, message);
    }
}

fn rotate(file: &Path) {
    let old = PathBuf::from(format!("{}.1", file.display()));
    let _ = fs::remove_file(&old);
    let _ = fs::rename(file, &old);
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`, matching `Date.toISOString()`.
pub fn format_timestamp(now_ms: i64) -> String {
    let seconds = now_ms.div_euclid(1000);
    let millis = now_ms.rem_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = time / 3600;
    let minute = (time % 3600) / 60;
    let second = time % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's `civil_from_days`, used without a date crate.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntfy_relay_core::test_support::Scratch;

    fn temp_dir(name: &str) -> Scratch {
        Scratch::new(&format!("app-logger-{name}"))
    }

    #[test]
    fn timestamp_matches_iso_utc() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_timestamp(1_700_000_000_123),
            "2023-11-14T22:13:20.123Z"
        );
    }

    #[test]
    fn levels_filter_and_lines_carry_their_level() {
        let dir = temp_dir("levels");
        let file = dir.join("relay.log");
        let logger = Logger::new(Some(file.clone()), LogLevel::Info, false, 1024);
        logger.init().unwrap();
        logger.debug("hidden");
        logger.info("visible");
        logger.error("also visible");
        logger.flush();
        let text = fs::read_to_string(&file).unwrap();
        assert!(!text.contains("hidden"));
        assert!(text.contains(" INFO visible"));
        assert!(text.contains(" ERROR also visible"));
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn ten_thousand_lines_are_written_through_one_open_handle() {
        let dir = temp_dir("volume");
        let file = dir.join("relay.log");
        let logger = Logger::new(Some(file.clone()), LogLevel::Info, false, 64 * 1024 * 1024);
        logger.init().unwrap();
        for index in 0..10_000 {
            logger.info(&format!("line {index}"));
        }
        logger.flush();
        let text = fs::read_to_string(&file).unwrap();
        assert_eq!(text.lines().count(), 10_000);
        assert!(text.ends_with("line 9999\n"));
    }

    #[test]
    fn a_hung_writer_does_not_hold_the_drop_past_the_deadline() {
        let worker = thread::spawn(|| thread::sleep(Duration::from_secs(10)));
        let started = std::time::Instant::now();
        join_with_deadline(worker, Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn rotation_keeps_one_previous_file() {
        let dir = temp_dir("rotate");
        let file = dir.join("relay.log");
        let logger = Logger::new(Some(file.clone()), LogLevel::Info, false, 60);
        logger.init().unwrap();
        for index in 0..10 {
            logger.info(&format!("line {index} with some padding"));
        }
        logger.flush();
        assert!(dir.join("relay.log.1").exists());
        assert!(fs::metadata(&file).unwrap().len() < 200);
    }

    #[cfg(unix)]
    #[test]
    fn log_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let probe = dir.join("probe");
        fs::write(&probe, b"x").unwrap();
        if fs::metadata(&probe).unwrap().permissions().mode() & 0o022 != 0 {
            return;
        }
        let file = dir.join("relay.log");
        let logger = Logger::new(Some(file.clone()), LogLevel::Info, false, 1024);
        logger.init().unwrap();
        logger.info("line");
        logger.flush();
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
