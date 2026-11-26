//! ntfy event subscription: one SSE connection, automatic resubscribe.
//!
//! Mirrors `Relay.subscribe` in `ntfy-inbox-relay.mjs`: replay cursor with a
//! 60 s overlap, 30 s header bound, 1-60 s exponential backoff with jitter,
//! backoff reset after a stable minute, and an immediate resubscribe when the
//! discovered topic set changes. Frames enter the bounded inbound queue; the
//! main loop drains them one at a time.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use ntfy_relay_inbound::filter::InboundMessage;
use ntfy_relay_inbound::queue::MessageQueue;
use ntfy_relay_ntfy::http::{HttpRequest, Method};
use ntfy_relay_ntfy::publish::{sse_url, NtfyServer};
use ntfy_relay_ntfy::sse::SseParser;
use ntfy_relay_ntfy::stream::{StreamResponse, StreamTransport};
use tokio::sync::{watch, Mutex, Notify};

use crate::discovery::MAX_TOPICS;
use crate::logger::Logger;
use crate::util::{clip, jitter, now_ms};

pub const SSE_HEADER_TIMEOUT: Duration = Duration::from_secs(30);
pub const STABLE_CONNECTION: Duration = Duration::from_secs(60);
pub const RECONNECT_OVERLAP_SECONDS: i64 = 60;
/// A 2xx stream that yields nothing for this long is dead; twice the ntfy
/// keepalive default of 45 s.
pub const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// A non-2xx body may not hold the reader open.
pub const SSE_DETAIL_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_START_MS: i64 = 1_000;
const BACKOFF_MAX_MS: i64 = 60_000;
/// Consecutive 401/403 answers before the relay reports `sse=refused`.
const REFUSED_AFTER: u32 = 5;

pub struct SseTask {
    pub server: NtfyServer,
    pub token: String,
    pub replay_window_minutes: f64,
    pub logger: Logger,
    pub transport: Arc<dyn StreamTransport>,
    pub queue: Arc<Mutex<MessageQueue>>,
    pub notify: Arc<Notify>,
    pub queue_len: Arc<AtomicUsize>,
    pub last_processed: Arc<AtomicI64>,
    pub last_event_at: Arc<AtomicI64>,
    /// Called once when the relay can be called ready: the first accepted
    /// subscription, or an empty topic list where there is nothing to subscribe.
    pub ready: Arc<dyn Fn() + Send + Sync>,
    /// Set after five consecutive 401/403 answers; cleared on a 2xx stream.
    pub refused: Arc<AtomicBool>,
}

enum Wait {
    Timeout,
    Topics,
    Stop,
}

impl SseTask {
    pub async fn run(
        self,
        mut topics: watch::Receiver<Vec<String>>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut backoff = BACKOFF_START_MS;
        let mut ready = false;
        let mut auth_failures = 0u32;
        loop {
            let current = topics.borrow_and_update().clone();
            if current.is_empty() {
                if !ready {
                    ready = true;
                    (self.ready)();
                }
                if matches!(
                    self.wait(5_000, &mut topics, &mut shutdown).await,
                    Wait::Stop
                ) {
                    return;
                }
                continue;
            }
            if current.len() > MAX_TOPICS {
                self.logger.error(&format!(
                    "refusing to subscribe to {} topics",
                    current.len()
                ));
                if matches!(
                    self.wait(BACKOFF_MAX_MS, &mut topics, &mut shutdown).await,
                    Wait::Stop
                ) {
                    return;
                }
                continue;
            }

            let since = self.since();
            let names: Vec<&str> = current.iter().map(String::as_str).collect();
            let url = sse_url(&self.server, &names, since);
            let request = HttpRequest::new(Method::Get, url)
                .header("Authorization", format!("Bearer {}", self.token));
            self.logger
                .info(&format!("subscribing {} topics since={since}", names.len()));

            let mut response = tokio::select! {                result = self.transport.open(request, SSE_HEADER_TIMEOUT) => match result {
                    Ok(response) => response,
                    Err(error) => {
                        if self.retry(&mut backoff, &mut topics, &mut shutdown, &error.to_string()).await {
                            return;
                        }
                        continue;
                    }
                },
                _ = topics.changed() => {
                    self.logger.info("resubscribing after topic change");
                    continue;
                }
                _ = shutdown.changed() => return,
            };

            if !(200..300).contains(&response.status) {
                let status = response.status;
                let detail = read_detail(response).await;
                let message = if detail.is_empty() {
                    format!("sse HTTP {status}")
                } else {
                    format!("sse HTTP {status} {}", clip(&detail, 200))
                };
                if status == 401 || status == 403 {
                    auth_failures += 1;
                    if auth_failures == REFUSED_AFTER && !self.refused.swap(true, Ordering::Relaxed)
                    {
                        self.logger.error(&format!(
                            "sse: the token was refused {REFUSED_AFTER} times; status shows sse=refused"
                        ));
                    }
                } else {
                    auth_failures = 0;
                }
                if status < 500 && status != 429 {
                    self.logger.error(&format!("subscribe failed: {message}"));
                    if matches!(
                        self.wait(BACKOFF_MAX_MS, &mut topics, &mut shutdown).await,
                        Wait::Stop
                    ) {
                        return;
                    }
                    continue;
                }
                if self
                    .retry(&mut backoff, &mut topics, &mut shutdown, &message)
                    .await
                {
                    return;
                }
                continue;
            }

            self.logger.info("subscribed");
            auth_failures = 0;
            if self.refused.swap(false, Ordering::Relaxed) {
                self.logger.info("sse: the token is accepted again");
            }
            if !ready {
                ready = true;
                (self.ready)();
            }
            let connected_at = Instant::now();
            let mut resubscribe = false;
            let mut failure = None;
            let mut parser = SseParser::new();
            let mut overflow_logged = false;
            let idle = tokio::time::sleep(SSE_IDLE_TIMEOUT);
            tokio::pin!(idle);
            loop {
                tokio::select! {
                    chunk = response.chunks.next() => {
                        idle.as_mut().reset(tokio::time::Instant::now() + SSE_IDLE_TIMEOUT);
                        match chunk {
                        Some(Ok(bytes)) => {
                            let events = parser.push(&bytes);
                            if let Some(kind) = parser.take_overflow() {
                                if !overflow_logged {
                                    self.logger.warn(&format!(
                                        "sse frame dropped: {} cap exceeded",
                                        overflow_name(kind)
                                    ));
                                    overflow_logged = true;
                                }
                            }
                            for value in events {
                                self.enqueue(value).await;
                            }
                        }
                        Some(Err(error)) => {
                            failure = Some(error.to_string());
                            break;
                        }
                        None => {
                            let events = parser.finish();
                            if let Some(kind) = parser.take_overflow() {
                                if !overflow_logged {
                                    self.logger.warn(&format!(
                                        "sse frame dropped: {} cap exceeded",
                                        overflow_name(kind)
                                    ));
                                }
                            }
                            for value in events {
                                self.enqueue(value).await;
                            }
                            failure = Some("sse stream ended".to_string());
                            break;
                        }
                        }
                    },
                    _ = &mut idle => {
                        failure = Some(format!(
                            "sse stream idle for {} s",
                            SSE_IDLE_TIMEOUT.as_secs()
                        ));
                        break;
                    }
                    _ = topics.changed() => {
                        resubscribe = true;
                        break;
                    }
                    _ = shutdown.changed() => return,
                }
            }
            if resubscribe {
                self.logger.info("resubscribing after topic change");
                continue;
            }
            if connected_at.elapsed() >= STABLE_CONNECTION {
                backoff = BACKOFF_START_MS;
            }
            let failure = failure.unwrap_or_else(|| "sse stream ended".to_string());
            if self
                .retry(&mut backoff, &mut topics, &mut shutdown, &failure)
                .await
            {
                return;
            }
        }
    }

    fn since(&self) -> i64 {
        let last = self.last_processed.load(Ordering::Relaxed);
        if last > 0 {
            last - RECONNECT_OVERLAP_SECONDS
        } else {
            now_ms().div_euclid(1000) - (self.replay_window_minutes * 60.0) as i64
        }
    }

    async fn enqueue(&self, value: serde_json::Value) {
        self.last_event_at.store(now_ms(), Ordering::Relaxed);
        let message: InboundMessage = match serde_json::from_value(value) {
            Ok(message) => message,
            Err(error) => {
                self.logger.debug(&format!("sse frame dropped: {error}"));
                return;
            }
        };
        let received_at = now_ms();
        let dropped = {
            let mut queue = self.queue.lock().await;
            let dropped = queue.push(message, received_at);
            self.queue_len.store(queue.len(), Ordering::Relaxed);
            dropped
        };
        if let Some(dropped) = dropped {
            self.logger.warn(&format!(
                "message queue full, dropped oldest id={} topic={}",
                dash_if_empty(&dropped.message.id),
                dash_if_empty(&dropped.message.topic)
            ));
        }
        self.notify.notify_one();
    }

    /// Logs one reconnect line and waits. Returns true when shutdown won.
    async fn retry(
        &self,
        backoff: &mut i64,
        topics: &mut watch::Receiver<Vec<String>>,
        shutdown: &mut watch::Receiver<bool>,
        message: &str,
    ) -> bool {
        let wait = jitter(*backoff);
        self.logger.warn(&format!(
            "subscribe failed: {}; retry in {wait} ms",
            clip(message, 300)
        ));
        let stopped = matches!(self.wait(wait, topics, shutdown).await, Wait::Stop);
        *backoff = (*backoff * 2).min(BACKOFF_MAX_MS);
        stopped
    }

    async fn wait(
        &self,
        millis: i64,
        topics: &mut watch::Receiver<Vec<String>>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Wait {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(millis.max(1) as u64)) => Wait::Timeout,
            _ = topics.changed() => Wait::Topics,
            _ = shutdown.changed() => Wait::Stop,
        }
    }
}

async fn read_detail(response: StreamResponse) -> String {
    read_detail_with(response, SSE_DETAIL_TIMEOUT).await
}

async fn read_detail_with(mut response: StreamResponse, timeout: Duration) -> String {
    match tokio::time::timeout(timeout, async {
        let mut buffer = Vec::new();
        while let Some(chunk) = response.chunks.next().await {
            let Ok(bytes) = chunk else {
                break;
            };
            let room = 200usize.saturating_sub(buffer.len());
            if room == 0 {
                break;
            }
            buffer.extend_from_slice(&bytes[..bytes.len().min(room)]);
        }
        String::from_utf8_lossy(&buffer).trim().to_string()
    })
    .await
    {
        Ok(detail) => detail,
        Err(_) => "detail timed out".to_string(),
    }
}

fn dash_if_empty(value: &str) -> &str {
    if value.is_empty() {
        "-"
    } else {
        value
    }
}

fn overflow_name(kind: ntfy_relay_ntfy::sse::SseOverflow) -> &'static str {
    match kind {
        ntfy_relay_ntfy::sse::SseOverflow::Line => "line",
        ntfy_relay_ntfy::sse::SseOverflow::Data => "data",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_util::stream;
    use ntfy_relay_core::config::LogLevel;
    use ntfy_relay_ntfy::http::HttpError;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    struct FakeStream {
        responses: StdMutex<VecDeque<StreamResponse>>,
        requests: StdMutex<Vec<String>>,
    }

    impl FakeStream {
        fn new(responses: Vec<StreamResponse>) -> Self {
            Self {
                responses: StdMutex::new(responses.into()),
                requests: StdMutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl StreamTransport for FakeStream {
        async fn open(
            &self,
            request: HttpRequest,
            _header_timeout: Duration,
        ) -> Result<StreamResponse, HttpError> {
            self.requests.lock().unwrap().push(request.url);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| HttpError::Transport("no response left".to_string()))
        }
    }

    fn response(parts: Vec<&'static [u8]>) -> StreamResponse {
        StreamResponse {
            status: 200,
            headers: Vec::new(),
            chunks: stream::iter(
                parts
                    .into_iter()
                    .map(|part| Ok(Bytes::from_static(part)))
                    .collect::<Vec<_>>(),
            )
            .boxed(),
        }
    }

    fn silent_response() -> StreamResponse {
        StreamResponse {
            status: 200,
            headers: Vec::new(),
            chunks: stream::pending::<Result<Bytes, HttpError>>().boxed(),
        }
    }

    fn status_response(status: u16) -> StreamResponse {
        StreamResponse {
            status,
            headers: Vec::new(),
            chunks: stream::empty::<Result<Bytes, HttpError>>().boxed(),
        }
    }

    fn task(transport: Arc<dyn StreamTransport>) -> SseTask {
        SseTask {
            server: NtfyServer::parse("https://ntfy.test").unwrap(),
            token: "token-123456".to_string(),
            replay_window_minutes: 60.0,
            logger: Logger::new(None, LogLevel::Debug, false, 1024),
            transport,
            queue: Arc::new(Mutex::new(MessageQueue::new())),
            notify: Arc::new(Notify::new()),
            queue_len: Arc::new(AtomicUsize::new(0)),
            last_processed: Arc::new(AtomicI64::new(0)),
            last_event_at: Arc::new(AtomicI64::new(0)),
            ready: Arc::new(|| {}),
            refused: Arc::new(AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn a_frame_reaches_the_queue_and_shutdown_ends_the_task() {
        let fake = Arc::new(FakeStream::new(vec![response(vec![
            b"data: {\"id\":\"m1\",\"event\":\"message\",\"topic\":\"oc-x\",\"time\":1,\"message\":\"hi\"}\n\n",
        ])]));
        let sse = task(fake.clone());
        let queue = sse.queue.clone();
        let (topics_tx, topics_rx) = watch::channel(vec!["oc-x".to_string()]);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(sse.run(topics_rx, shutdown_rx));

        let received = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let next = { queue.lock().await.pop() };
                if let Some(item) = next {
                    return item;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(received.message.id, "m1");
        assert_eq!(received.message.topic, "oc-x");

        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
        drop(topics_tx);
        assert_eq!(fake.requests.lock().unwrap().len(), 1);
        assert!(fake.requests.lock().unwrap()[0].contains("/oc-x/sse?since="));
    }

    #[tokio::test]
    async fn malformed_frames_are_dropped() {
        let fake = Arc::new(FakeStream::new(vec![response(vec![b"data: 5\n\n"])]));
        let sse = task(fake);
        let queue = sse.queue.clone();
        let (_topics_tx, topics_rx) = watch::channel(vec!["oc-x".to_string()]);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(sse.run(topics_rx, shutdown_rx));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(queue.lock().await.is_empty());
        shutdown_tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn a_stalled_error_body_times_out() {
        let response = StreamResponse {
            status: 500,
            headers: Vec::new(),
            chunks: stream::pending::<Result<Bytes, HttpError>>().boxed(),
        };
        let detail = read_detail_with(response, Duration::from_millis(20)).await;
        assert_eq!(detail, "detail timed out");
    }

    #[tokio::test]
    async fn an_empty_topic_list_marks_the_relay_ready() {
        let fake = Arc::new(FakeStream::new(vec![]));
        let mut sse = task(fake);
        let marked = Arc::new(AtomicBool::new(false));
        let flag = marked.clone();
        sse.ready = Arc::new(move || flag.store(true, Ordering::Relaxed));
        let (_topics_tx, topics_rx) = watch::channel(Vec::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(sse.run(topics_rx, shutdown_rx));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(marked.load(Ordering::Relaxed));
        shutdown_tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test(start_paused = true)]
    async fn five_auth_refusals_set_the_refused_flag() {
        let fake = Arc::new(FakeStream::new(
            (0..5).map(|_| status_response(403)).collect(),
        ));
        let sse = task(fake.clone());
        let refused = sse.refused.clone();
        let (_topics_tx, topics_rx) = watch::channel(vec!["oc-x".to_string()]);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(sse.run(topics_rx, shutdown_rx));
        for _ in 0..10 {
            tokio::time::advance(Duration::from_secs(61)).await;
            tokio::task::yield_now().await;
            if refused.load(Ordering::Relaxed) {
                break;
            }
        }
        assert!(refused.load(Ordering::Relaxed));
        assert!(fake.requests.lock().unwrap().len() >= 5);
        shutdown_tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_stream_reconnects_after_the_idle_deadline() {
        let fake = Arc::new(FakeStream::new(vec![
            silent_response(),
            response(vec![
                b"data: {\"id\":\"m1\",\"event\":\"message\",\"topic\":\"oc-x\",\"time\":1,\"message\":\"hi\"}\n\n",
            ]),
        ]));
        let sse = task(fake.clone());
        let (_topics_tx, topics_rx) = watch::channel(vec!["oc-x".to_string()]);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(sse.run(topics_rx, shutdown_rx));

        for _ in 0..200 {
            if !fake.requests.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(fake.requests.lock().unwrap().len(), 1);
        tokio::time::advance(SSE_IDLE_TIMEOUT + Duration::from_secs(1)).await;
        for _ in 0..20 {
            if fake.requests.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(fake.requests.lock().unwrap().len(), 2);
        shutdown_tx.send(true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
}
