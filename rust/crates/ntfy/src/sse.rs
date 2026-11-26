//! Incremental SSE parser.
//!
//! Mirrors `createSSEParser` in `ntfy-inbox-relay.mjs`: only `data` fields
//! matter, comment lines are ignored, multiple data lines join with `\n`, and
//! a frame whose payload is not valid JSON is dropped. The parser works on
//! bytes so a UTF-8 sequence or a CRLF pair may split at any chunk boundary.

use serde_json::Value;

/// One SSE line above this is dropped, together with the frame it belongs to.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// A frame whose joined `data` payload exceeds this is dropped.
pub const MAX_DATA_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseOverflow {
    Line,
    Data,
}

enum Line {
    Bytes(Vec<u8>),
    /// An oversized line was consumed and thrown away.
    Skipped,
    Pending,
}

pub struct SseParser {
    buffer: Vec<u8>,
    /// Read position in `buffer`; the consumed prefix is dropped once per
    /// chunk, so a chunk with many short lines is linear, not quadratic.
    cursor: usize,
    data: Vec<String>,
    data_len: usize,
    discarding_line: bool,
    discarding_frame: bool,
    overflow: Option<SseOverflow>,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            cursor: 0,
            data: Vec::new(),
            data_len: 0,
            discarding_line: false,
            discarding_frame: false,
            overflow: None,
        }
    }

    /// Consumes one network chunk and returns every frame completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Value> {
        self.compact();
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        loop {
            match self.take_line() {
                Line::Bytes(line) => self.consume_line(&line, &mut events),
                Line::Skipped => {}
                Line::Pending => break,
            }
        }
        events
    }

    /// Flushes a final line and frame. A bare trailing `\r` is stripped, as
    /// the Node parser does at end of stream.
    pub fn finish(&mut self) -> Vec<Value> {
        let mut events = Vec::new();
        self.compact();
        if self.discarding_line {
            self.buffer.clear();
            self.discarding_line = false;
        } else if !self.buffer.is_empty() {
            let mut line = std::mem::take(&mut self.buffer);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > MAX_LINE_BYTES {
                self.overflow.get_or_insert(SseOverflow::Line);
                self.discarding_frame = true;
            } else {
                self.consume_line(&line, &mut events);
            }
        }
        self.dispatch(&mut events);
        events
    }

    /// Returns the first cap hit since the last call. The caller logs it once
    /// per connection; the dropped frame is not retried.
    pub fn take_overflow(&mut self) -> Option<SseOverflow> {
        self.overflow.take()
    }

    /// Drops the consumed prefix. Called once per chunk, never per line.
    fn compact(&mut self) {
        if self.cursor > 0 {
            self.buffer.drain(..self.cursor);
            self.cursor = 0;
        }
    }

    fn take_line(&mut self) -> Line {
        let rest = &self.buffer[self.cursor..];
        if self.discarding_line {
            match find_terminator(rest) {
                Some((_, consumed)) => {
                    self.cursor += consumed;
                    self.discarding_line = false;
                    Line::Skipped
                }
                None => {
                    if rest.len() > MAX_LINE_BYTES {
                        self.buffer.clear();
                        self.cursor = 0;
                    }
                    Line::Pending
                }
            }
        } else {
            match find_terminator(rest) {
                Some((index, consumed)) => {
                    let line = rest[..index].to_vec();
                    self.cursor += consumed;
                    if line.len() > MAX_LINE_BYTES {
                        self.overflow.get_or_insert(SseOverflow::Line);
                        self.discarding_frame = true;
                        Line::Skipped
                    } else {
                        Line::Bytes(line)
                    }
                }
                None => {
                    if rest.len() > MAX_LINE_BYTES {
                        self.buffer.clear();
                        self.cursor = 0;
                        self.discarding_line = true;
                        self.overflow.get_or_insert(SseOverflow::Line);
                        self.discarding_frame = true;
                    }
                    Line::Pending
                }
            }
        }
    }

    fn consume_line(&mut self, line: &[u8], events: &mut Vec<Value>) {
        let Ok(line) = std::str::from_utf8(line) else {
            return;
        };
        if line.is_empty() {
            self.dispatch(events);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.find(':') {
            Some(colon) => (&line[..colon], &line[colon + 1..]),
            None => (line, ""),
        };
        if field != "data" || self.discarding_frame {
            return;
        }
        let value = value.strip_prefix(' ').unwrap_or(value);
        if self.data_len + value.len() + 1 > MAX_DATA_BYTES {
            self.data.clear();
            self.data_len = 0;
            self.discarding_frame = true;
            self.overflow.get_or_insert(SseOverflow::Data);
            return;
        }
        self.data_len += value.len() + 1;
        self.data.push(value.to_string());
    }

    fn dispatch(&mut self, events: &mut Vec<Value>) {
        let discard = self.discarding_frame;
        self.discarding_frame = false;
        if self.data.is_empty() {
            return;
        }
        let raw = self.data.join("\n");
        self.data.clear();
        self.data_len = 0;
        if discard {
            return;
        }
        if let Ok(value) = serde_json::from_str(&raw) {
            events.push(value);
        }
    }
}

/// Position of the line terminator and its byte length. A trailing bare `\r`
/// may still become a CRLF, so it does not terminate a line yet.
fn find_terminator(buffer: &[u8]) -> Option<(usize, usize)> {
    for (index, byte) in buffer.iter().enumerate() {
        match byte {
            b'\n' => return Some((index, index + 1)),
            b'\r' => {
                if index + 1 == buffer.len() {
                    return None;
                }
                let consumed = if buffer[index + 1] == b'\n' {
                    index + 2
                } else {
                    index + 1
                };
                return Some((index, consumed));
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reassembles_split_chunks_and_ignores_comments() {
        let mut parser = SseParser::new();
        let mut events = parser.push(b"data: {\"id\":\"a\",");
        events.extend(parser.push(b"\"event\":\"message\"}\r\n\r\n: keepalive\r\n"));
        events.extend(parser.push(b"data: {\"event\":\"keepalive\"}\n\n"));
        events.extend(parser.finish());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], json!({"id": "a", "event": "message"}));
        assert_eq!(events[1], json!({"event": "keepalive"}));
    }

    #[test]
    fn crlf_split_at_every_byte_produces_one_event() {
        let payload = b"data: {\"a\":1,\r\ndata: \"b\":2}\r\n\r\n";
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for byte in payload {
            events.extend(parser.push(std::slice::from_ref(byte)));
        }
        events.extend(parser.finish());
        assert_eq!(events, vec![json!({"a": 1, "b": 2})]);
    }

    #[test]
    fn lone_cr_frames_are_separate_events() {
        let mut parser = SseParser::new();
        let mut events = parser.push(b"data: {\"a\":1}\r\rdata: {\"a\":2}\r\r");
        events.extend(parser.finish());
        assert_eq!(events, vec![json!({"a": 1}), json!({"a": 2})]);
    }

    #[test]
    fn invalid_json_and_utf8_are_dropped() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: not-json\n\ndata: \xff\xfe\n\ndata: {\"ok\":true}\n\n");
        assert_eq!(events, vec![json!({"ok": true})]);
    }

    #[test]
    fn multiline_data_joins_with_newline() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: {\"a\":1,\ndata: \"b\":2}\n\n");
        assert_eq!(events, vec![json!({"a": 1, "b": 2})]);
    }

    #[test]
    fn finish_flushes_a_frame_without_a_blank_line() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: {\"a\":1}").is_empty());
        assert_eq!(parser.finish(), vec![json!({"a": 1})]);
    }

    #[test]
    fn empty_data_field_is_not_a_frame() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data:\n\n").is_empty());
    }

    #[test]
    fn many_small_frames_in_one_chunk_stay_linear() {
        let frames = 50_000;
        let mut payload = Vec::with_capacity(frames * 16);
        for _ in 0..frames {
            payload.extend_from_slice(b"data: {\"n\":1}\n\n");
        }
        let mut parser = SseParser::new();
        let started = std::time::Instant::now();
        let events = parser.push(&payload);
        assert_eq!(events.len(), frames);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "one chunk with {frames} frames took {:?}",
            started.elapsed()
        );
        assert_eq!(parser.cursor, parser.buffer.len());
    }

    #[test]
    fn a_split_line_still_completes_after_compaction() {
        let mut parser = SseParser::new();
        assert_eq!(
            parser.push(b"data: {\"a\":1}\n\ndata: {\"b\":"),
            vec![json!({"a": 1})]
        );
        assert_eq!(parser.push(b"2}\n\n"), vec![json!({"b": 2})]);
    }

    #[test]
    fn an_oversize_line_is_dropped_and_reported() {
        let mut parser = SseParser::new();
        let mut chunk = Vec::with_capacity(MAX_LINE_BYTES + 64);
        chunk.extend_from_slice(b"data: ");
        chunk.extend_from_slice(&vec![b'a'; MAX_LINE_BYTES]);
        chunk.extend_from_slice(b"\n\ndata: {\"ok\":true}\n\n");
        assert_eq!(parser.push(&chunk), vec![json!({"ok": true})]);
        assert_eq!(parser.take_overflow(), Some(SseOverflow::Line));
        assert_eq!(parser.take_overflow(), None);
    }

    #[test]
    fn a_frame_with_an_oversize_line_drops_all_its_data() {
        let mut parser = SseParser::new();
        let mut chunk = b"data: {\"a\":1}\ndata: ".to_vec();
        chunk.extend_from_slice(&vec![b'a'; MAX_LINE_BYTES]);
        chunk.extend_from_slice(b"\n\ndata: {\"ok\":true}\n\n");
        assert_eq!(parser.push(&chunk), vec![json!({"ok": true})]);
        assert_eq!(parser.take_overflow(), Some(SseOverflow::Line));
    }

    #[test]
    fn an_oversize_frame_is_dropped_until_the_blank_line() {
        let mut parser = SseParser::new();
        let piece = format!("data: {}\n", "a".repeat(512 * 1024));
        let mut chunk = Vec::new();
        for _ in 0..17 {
            chunk.extend_from_slice(piece.as_bytes());
        }
        chunk.extend_from_slice(b"\ndata: {\"ok\":true}\n\n");
        assert_eq!(parser.push(&chunk), vec![json!({"ok": true})]);
        assert_eq!(parser.take_overflow(), Some(SseOverflow::Data));
    }

    #[test]
    fn a_line_that_never_ends_stays_bounded() {
        let mut parser = SseParser::new();
        assert!(parser.push(&vec![b'x'; MAX_LINE_BYTES * 2]).is_empty());
        assert!(parser.buffer.len() <= MAX_LINE_BYTES);
        assert!(parser.push(b"\n\n").is_empty());
        assert_eq!(parser.take_overflow(), Some(SseOverflow::Line));
    }
}
