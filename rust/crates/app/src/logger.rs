//! File logger with one rotated copy.
//!
//! Mirrors the Node `Logger`: ISO timestamp, level, message, append per line,
//! rotate at 5 MB into `<file>.1`, and never let a write failure stop the
//! relay. Echo is off in the deployed daemon because it runs hidden.
//!
//! One writer thread owns the file. Callers format and enqueue a line; the
//! runtime thread never opens, appends, or rotates, so a slow disk or a rotate
//! cannot stall the select loop. The thread ends, after draining, when the last
//! logger clone drops; `flush` waits for the queue.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use ntfy_relay_core::config::LogLevel;

pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

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
            let _ = worker.join();
        }
    }
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

    /// Creates the log directory and records the current file size.
    pub fn init(&self) -> io::Result<()> {
        let Some(file) = &self.inner.file else {
            return Ok(());
        };
        if let Some(parent) = file.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let size = fs::metadata(file).map(|meta| meta.len()).unwrap_or(0);
        #[cfg(unix)]
        if size > 0 {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(file, fs::Permissions::from_mode(0o600));
        }
        #[cfg(not(unix))]
        let _ = size;
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

fn writer_loop(file: Option<PathBuf>, max_bytes: u64, receiver: Receiver<Command>) {
    let mut size: Option<u64> = None;
    for command in receiver {
        match command {
            Command::Line(line) => {
                let Some(file) = &file else {
                    continue;
                };
                let mut current = size.unwrap_or_else(|| {
                    if let Some(parent) = file.parent() {
                        if !parent.as_os_str().is_empty() {
                            let _ = fs::create_dir_all(parent);
                        }
                    }
                    fs::metadata(file).map(|meta| meta.len()).unwrap_or(0)
                });
                if current >= max_bytes {
                    rotate(file);
                    current = 0;
                }
                let mut options = OpenOptions::new();
                options.create(true).append(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                match options.open(file) {
                    Ok(mut handle) => {
                        let written = format!("{line}\n");
                        if let Err(error) = handle.write_all(written.as_bytes()) {
                            eprintln!("log write failed: {error}");
                        } else {
                            current += written.len() as u64;
                        }
                    }
                    Err(error) => eprintln!("log write failed: {error}"),
                }
                size = Some(current);
            }
            Command::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
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

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("app-logger-tests")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
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
