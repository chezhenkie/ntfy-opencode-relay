//! Bounded inbound queue.
//!
//! The SSE reader never awaits the handler. Messages enter here in arrival
//! order and drain one at a time. Overflow drops the oldest entry; the replay
//! cursor is not touched, so a dropped message is replayed after a reconnect
//! only if it is still inside the replay window.
//!
//! Mirrors `Relay.enqueue` in `ntfy-inbox-relay.mjs`.

use std::collections::VecDeque;

use crate::filter::InboundMessage;

pub const MAX_QUEUE: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedMessage {
    pub message: InboundMessage,
    /// Wall-clock milliseconds when the frame arrived. Staleness is judged
    /// here, not when the handler eventually runs.
    pub received_at: i64,
}

#[derive(Debug, Default)]
pub struct MessageQueue {
    items: VecDeque<QueuedMessage>,
    cap: usize,
}

impl MessageQueue {
    pub fn new() -> Self {
        Self::with_capacity(MAX_QUEUE)
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            items: VecDeque::with_capacity(cap.min(64)),
            cap: cap.max(1),
        }
    }

    /// Push one message. When the queue is full the oldest entry is returned
    /// so the caller can log it.
    pub fn push(&mut self, message: InboundMessage, received_at: i64) -> Option<QueuedMessage> {
        let dropped = if self.items.len() >= self.cap {
            self.items.pop_front()
        } else {
            None
        };
        self.items.push_back(QueuedMessage {
            message,
            received_at,
        });
        dropped
    }

    pub fn pop(&mut self) -> Option<QueuedMessage> {
        self.items.pop_front()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Drop everything and report how many entries were discarded.
    pub fn clear(&mut self) -> usize {
        let dropped = self.items.len();
        self.items.clear();
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str) -> InboundMessage {
        InboundMessage {
            id: id.to_string(),
            event: "message".to_string(),
            topic: "oc-x".to_string(),
            time: Some(100),
            message: "hi".to_string(),
            ..InboundMessage::default()
        }
    }

    #[test]
    fn overflow_drops_the_oldest_and_keeps_arrival_order() {
        let mut queue = MessageQueue::new();
        for index in 0..MAX_QUEUE {
            assert!(queue
                .push(message(&format!("m{index}")), 1_700_000_000_000)
                .is_none());
        }
        assert_eq!(queue.len(), MAX_QUEUE);
        assert_eq!(
            queue
                .push(message(&format!("m{MAX_QUEUE}")), 1_700_000_000_001)
                .unwrap()
                .message
                .id,
            "m0"
        );
        assert_eq!(queue.pop().unwrap().message.id, "m1");
        assert_eq!(queue.len(), MAX_QUEUE - 1);
    }

    #[test]
    fn push_stamps_arrival_time_and_clear_counts() {
        let mut queue = MessageQueue::with_capacity(2);
        queue.push(message("m1"), 11);
        queue.push(message("m2"), 12);
        assert_eq!(queue.pop().unwrap().received_at, 11);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.clear(), 1);
        assert!(queue.is_empty());
    }
}
