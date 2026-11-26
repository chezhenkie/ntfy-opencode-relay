//! One writer around `core::state`, shared by the message loop and the
//! control engine's `TargetSink`.
//!
//! Every mutation takes the same mutex, takes a snapshot, and hands it to one
//! writer thread that rewrites the whole file atomically. The runtime thread
//! never touches the state file, so a slow disk cannot stall the select loop;
//! snapshots are enqueued in mutation order, so the last write wins.
//!
//! `TargetSink` cannot return an error, so a failed write is logged and the
//! in-memory hint still changes; the next successful write persists it.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Mutex;
use std::thread;

use ntfy_relay_core::adapter::TargetSink;
use ntfy_relay_core::state::{write_state_atomic, State, TargetState};
use ntfy_relay_inbound::session::short_session_id;
use tokio::sync::oneshot;

use crate::logger::Logger;

enum Command {
    Write(PathBuf, State, Option<oneshot::Sender<Result<(), String>>>),
    Flush(oneshot::Sender<()>),
}

pub struct StateStore {
    file: PathBuf,
    state: Mutex<State>,
    writer: mpsc::Sender<Command>,
    logger: Logger,
}

impl StateStore {
    pub fn new(file: PathBuf, state: State, logger: Logger) -> Self {
        let (writer, receiver) = mpsc::channel::<Command>();
        let writer_logger = logger.clone();
        thread::Builder::new()
            .name("state-writer".to_string())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    match command {
                        Command::Write(path, state, done) => {
                            let result = write_state_atomic(&path, &state)
                                .map_err(|error| error.to_string());
                            if let (Err(error), None) = (&result, &done) {
                                writer_logger.error(&format!("state write failed: {error}"));
                            }
                            if let Some(done) = done {
                                let _ = done.send(result);
                            }
                        }
                        Command::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })
            .expect("state writer thread");
        Self {
            file,
            state: Mutex::new(state),
            writer,
            logger,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Routing hints for one topic. Advisory: the resolver validates them.
    pub fn target(&self, topic: &str) -> TargetState {
        self.lock().target(topic).cloned().unwrap_or_default()
    }

    /// Session ids the relay remembers; the outbound backfill includes them.
    pub fn tracked_sessions(&self) -> Vec<String> {
        let state = self.lock();
        let mut ids = Vec::new();
        for target in state.targets.values() {
            ids.extend(target.last.clone());
            ids.extend(target.pending.clone());
        }
        ids.sort();
        ids.dedup();
        ids
    }

    /// `topic=shortid` list, or `none`. Used by `/status`.
    pub fn target_summary(&self) -> String {
        let state = self.lock();
        if state.targets.is_empty() {
            return "none".to_string();
        }
        state
            .targets
            .iter()
            .map(|(topic, target)| {
                let mut parts = Vec::new();
                if let Some(last) = &target.last {
                    parts.push(format!("{}={}", topic, short_session_id(last)));
                }
                if let Some(pending) = &target.pending {
                    parts.push(format!("{}=next:{}", topic, short_session_id(pending)));
                }
                parts.join(" ")
            })
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Replay cursor for one topic at queue time.
    pub fn topic_cursor(&self, topic: &str) -> (Vec<String>, i64) {
        let state = self.lock();
        match state.topics.get(topic) {
            Some(entry) => (entry.last_ids.clone(), entry.last_time),
            None => (Vec::new(), 0),
        }
    }

    /// State-before-delivery: the caller persists the accepted id first, so a
    /// crash cannot deliver the same message twice. The write runs on the
    /// writer thread; the snapshot is enqueued before the mutex is released.
    pub async fn record_processed(&self, topic: &str, id: &str, time: i64) -> io::Result<()> {
        let (done, finished) = oneshot::channel();
        {
            let mut state = self.lock();
            state.record_processed(topic, id, time);
            let snapshot = state.clone();
            self.writer
                .send(Command::Write(self.file.clone(), snapshot, Some(done)))
                .map_err(|_| io::Error::other("state writer stopped"))?;
        }
        match finished.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(io::Error::other(error)),
            Err(_) => Err(io::Error::other("state writer stopped")),
        }
    }

    /// Waits until every queued state write has finished.
    pub async fn flush(&self) {
        let (done, finished) = oneshot::channel();
        if self.writer.send(Command::Flush(done)).is_err() {
            return;
        }
        let _ = finished.await;
    }
}

impl TargetSink for StateStore {
    fn set_last(&self, topic: &str, session_id: &str, at: i64) {
        self.write(|state| state.set_last(topic, session_id, at));
    }

    fn set_pending(&self, topic: &str, session_id: Option<&str>, at: i64) {
        self.write(|state| state.set_pending(topic, session_id, at));
    }
}

impl StateStore {
    fn write(&self, mutate: impl FnOnce(&mut State)) {
        let mut state = self.lock();
        mutate(&mut state);
        let snapshot = state.clone();
        if self
            .writer
            .send(Command::Write(self.file.clone(), snapshot, None))
            .is_err()
        {
            self.logger
                .error("state write failed after target change: writer stopped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntfy_relay_core::config::LogLevel;
    use ntfy_relay_core::state::read_state;
    use ntfy_relay_core::test_support::Scratch;
    use std::sync::Arc;

    fn temp_dir(name: &str) -> Scratch {
        Scratch::new(&format!("app-store-{name}"))
    }

    fn store(name: &str) -> (Scratch, Arc<StateStore>) {
        let dir = temp_dir(name);
        let logger = Logger::new(None, LogLevel::Info, false, 1024);
        let store = Arc::new(StateStore::new(
            dir.join("state.json"),
            State::new(),
            logger,
        ));
        (dir, store)
    }

    #[tokio::test]
    async fn targets_persist_through_the_sink() {
        let (_dir, store) = store("targets");
        store.set_last("oc-x", "ses_123456789", 100);
        store.set_pending("oc-x", Some("ses_987654321"), 110);
        store.flush().await;
        let target = store.target("oc-x");
        assert_eq!(target.last.as_deref(), Some("ses_123456789"));
        assert_eq!(target.pending.as_deref(), Some("ses_987654321"));
        assert_eq!(store.target_summary(), "oc-x=ses_1234 oc-x=next:ses_9876");

        let saved = read_state(&store.file).unwrap();
        assert_eq!(
            saved.target("oc-x").unwrap().last.as_deref(),
            Some("ses_123456789")
        );

        store.set_pending("oc-x", None, 120);
        store.flush().await;
        assert_eq!(store.target("oc-x").pending, None);
        assert_eq!(store.tracked_sessions(), vec!["ses_123456789".to_string()]);
    }

    #[tokio::test]
    async fn cursor_reads_back_after_a_record() {
        let (_dir, store) = store("cursor");
        store.record_processed("oc-x", "m1", 100).await.unwrap();
        store.record_processed("oc-x", "m2", 90).await.unwrap();
        let (ids, last_time) = store.topic_cursor("oc-x");
        assert_eq!(ids, vec!["m1".to_string(), "m2".to_string()]);
        assert_eq!(last_time, 100);

        let saved = read_state(&store.file).unwrap();
        assert_eq!(saved.topics["oc-x"].last_ids, ids);
    }
}
