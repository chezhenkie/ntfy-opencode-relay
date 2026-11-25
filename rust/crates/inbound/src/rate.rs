//! Fixed prompt rate: 30 prompts per minute per topic with a burst of 10.
//!
//! The limiter sits in front of the prompt path so a publisher cannot drive
//! unlimited agent runs. It is a token bucket refilled at 30 per minute; the
//! bucket caps at 10, so a burst is absorbed and the long-run rate holds.
//! Commands (`/status`, `/steer`, permission replies) do not consume a token.

use std::collections::HashMap;

pub const PROMPTS_PER_MINUTE: u32 = 30;
pub const PROMPT_BURST: u32 = 10;
const WINDOW_MS: i64 = 60_000;
/// Buckets kept for topics that stopped sending; enough for the 150-topic cap.
const MAX_BUCKETS: usize = 256;

struct Bucket {
    tokens: f64,
    last_ms: i64,
}

pub struct PromptRate {
    buckets: HashMap<String, Bucket>,
}

impl Default for PromptRate {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptRate {
    pub fn new() -> Self {
        Self {
            buckets: HashMap::new(),
        }
    }

    /// Takes one token when the topic has one left. `now_ms` is monotonic.
    pub fn allow(&mut self, topic: &str, now_ms: i64) -> bool {
        if self.buckets.len() > MAX_BUCKETS {
            self.prune(now_ms);
        }
        let refill_per_ms = f64::from(PROMPTS_PER_MINUTE) / WINDOW_MS as f64;
        let cap = f64::from(PROMPT_BURST);
        let bucket = self
            .buckets
            .entry(topic.to_string())
            .or_insert_with(|| Bucket {
                tokens: cap,
                last_ms: now_ms,
            });
        let elapsed = (now_ms - bucket.last_ms).max(0);
        bucket.tokens = (bucket.tokens + elapsed as f64 * refill_per_ms).min(cap);
        bucket.last_ms = now_ms;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Drops buckets that have been idle past the refill window, keeping the
    /// map bounded; an idle bucket is re-created with a full burst.
    pub fn prune(&mut self, now_ms: i64) {
        if self.buckets.len() <= MAX_BUCKETS {
            return;
        }
        self.buckets
            .retain(|_, bucket| now_ms - bucket.last_ms < WINDOW_MS);
    }

    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_of_ten_passes_and_the_eleventh_waits() {
        let mut rate = PromptRate::new();
        let now = 1_000_000;
        for index in 0..PROMPT_BURST {
            assert!(rate.allow("proj", now), "token {index}");
        }
        assert!(!rate.allow("proj", now));
        assert!(rate.allow("proj", now + 2_000), "one token per two seconds");
        assert!(!rate.allow("proj", now + 2_000));
    }

    #[test]
    fn a_minute_of_idle_restores_the_burst() {
        let mut rate = PromptRate::new();
        rate.allow("proj", 1_000);
        for _ in 0..PROMPT_BURST {
            rate.allow("proj", 1_000);
        }
        assert!(!rate.allow("proj", 1_000));
        for _ in 0..PROMPT_BURST {
            assert!(rate.allow("proj", 61_000));
        }
    }

    #[test]
    fn topics_have_separate_buckets() {
        let mut rate = PromptRate::new();
        for _ in 0..PROMPT_BURST {
            assert!(rate.allow("one", 1_000));
        }
        assert!(!rate.allow("one", 1_000));
        assert!(rate.allow("two", 1_000));
    }

    #[test]
    fn the_bucket_map_is_bounded() {
        let mut rate = PromptRate::new();
        for index in 0..(MAX_BUCKETS + 50) {
            rate.allow(&format!("t{index}"), 1_000);
        }
        assert!(rate.len() > MAX_BUCKETS);
        rate.prune(1_000 + WINDOW_MS * 2);
        assert!(rate.len() <= MAX_BUCKETS);
    }
}
