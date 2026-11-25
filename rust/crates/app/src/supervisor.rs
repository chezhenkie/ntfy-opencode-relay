//! Restart policy for the daemon's two long-running tasks.
//!
//! `docs/specsheet/04-rust-relay-decisions.md` row 7 decides one process with two
//! supervised tasks. A task that ends without a shutdown signal is logged and
//! restarted in place, so the other direction and the inbound queue keep
//! running and the launcher sees no exit.

use std::future::Future;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::logger::Logger;

pub const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
pub const STABLE_AFTER: Duration = Duration::from_secs(60);
pub const TASK_STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    pub initial: Duration,
    pub max: Duration,
    pub stable_after: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            initial: INITIAL_BACKOFF,
            max: MAX_BACKOFF,
            stable_after: STABLE_AFTER,
        }
    }
}

impl RestartPolicy {
    /// Delay before the next attempt. A task that ran long enough resets the
    /// backoff, mirroring the SSE connection's own stable-connection rule.
    fn delay_after(&self, ran: Duration, next: &mut Duration) -> Duration {
        if ran >= self.stable_after {
            *next = self.initial;
        }
        let delay = (*next).min(self.max);
        *next = next.saturating_mul(2).min(self.max);
        delay
    }
}

/// Spawns a task that is restarted on any end other than shutdown. The factory
/// is called once per attempt, so every attempt gets fresh receivers from the
/// shared watch channels. A panicking task is caught by its join handle.
pub fn spawn<F, Fut>(
    name: &'static str,
    logger: Logger,
    mut shutdown: watch::Receiver<bool>,
    policy: RestartPolicy,
    mut factory: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    tokio::spawn(async move {
        let mut next_delay = policy.initial;
        loop {
            if *shutdown.borrow() {
                return;
            }
            let started = Instant::now();
            let mut handle = tokio::spawn(factory());
            let ended = tokio::select! {
                result = &mut handle => Some(result),
                _ = wait_stop(&mut shutdown) => None,
            };
            let Some(result) = ended else {
                // The task owns the same signal; give it the same grace the
                // composition root used before the supervisor existed.
                let _ = tokio::time::timeout(TASK_STOP_GRACE, &mut handle).await;
                return;
            };
            if *shutdown.borrow() {
                return;
            }
            let detail = match result {
                Ok(Ok(())) => "ended".to_string(),
                Ok(Err(error)) => format!("failed: {error}"),
                Err(error) => format!("panicked: {error}"),
            };
            let ran = started.elapsed();
            let delay = policy.delay_after(ran, &mut next_delay);
            logger.error(&format!(
                "{name} task {detail} after {} ms, restarting in {} ms",
                ran.as_millis(),
                delay.as_millis()
            ));
            let stopped = tokio::select! {
                _ = tokio::time::sleep(delay) => false,
                _ = wait_stop(&mut shutdown) => true,
            };
            if stopped {
                return;
            }
        }
    })
}

async fn wait_stop(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use ntfy_relay_core::config::LogLevel;

    fn logger() -> Logger {
        Logger::new(None, LogLevel::Error, false, 1024)
    }

    fn fast() -> RestartPolicy {
        RestartPolicy {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(4),
            stable_after: Duration::from_secs(60),
        }
    }

    #[test]
    fn backoff_doubles_and_resets_after_a_stable_run() {
        let mut next = Duration::from_millis(1);
        assert_eq!(
            fast().delay_after(Duration::from_millis(5), &mut next),
            Duration::from_millis(1)
        );
        assert_eq!(
            fast().delay_after(Duration::from_millis(5), &mut next),
            Duration::from_millis(2)
        );
        assert_eq!(
            fast().delay_after(Duration::from_millis(5), &mut next),
            Duration::from_millis(4)
        );
        assert_eq!(
            fast().delay_after(Duration::from_millis(5), &mut next),
            Duration::from_millis(4)
        );
        assert_eq!(
            fast().delay_after(Duration::from_secs(61), &mut next),
            Duration::from_millis(1)
        );
    }

    #[tokio::test]
    async fn failure_restarts_and_shutdown_stops() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let handle = spawn("test", logger(), shutdown_rx, fast(), move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err("boom".to_string())
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while attempts.load(Ordering::SeqCst) < 4 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("task restarts");
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("supervisor stops")
            .expect("join");
        let seen = attempts.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), seen);
    }

    #[tokio::test]
    async fn panic_restarts() {
        async fn boom(counter: Arc<AtomicUsize>) -> Result<(), String> {
            counter.fetch_add(1, Ordering::SeqCst);
            panic!("boom");
        }

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let handle = spawn("test", logger(), shutdown_rx, fast(), move || {
            boom(counter.clone())
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while attempts.load(Ordering::SeqCst) < 3 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("panicking task restarts");
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("supervisor stops")
            .expect("join");
    }

    #[tokio::test]
    async fn shutdown_before_start_runs_nothing() {
        let (shutdown_tx, shutdown_rx) = watch::channel(true);
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let handle = spawn("test", logger(), shutdown_rx, fast(), move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Ok::<(), String>(()) }
        });
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("supervisor stops")
            .expect("join");
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
        drop(shutdown_tx);
    }
}
