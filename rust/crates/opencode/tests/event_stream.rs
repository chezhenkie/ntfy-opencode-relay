//! Event contract test over a captured `/api/event` stream.
//!
//! The recording contains real frames from a short session run:
//! `server.connected`, `session.created`, `session.execution.started`, a
//! reasoning delta, `session.text.started`, `session.text.ended`,
//! `session.execution.succeeded`, and two `session.deleted` frames, plus one
//! heartbeat comment and one malformed frame. Only the routes `outbound` maps
//! should come back.

use ntfy_relay_opencode::event::{parse, Envelope, RuntimeEvent};

const STREAM: &str = include_str!("fixtures/event-stream.sse");

fn frames() -> Vec<serde_json::Value> {
    let mut events = Vec::new();
    let mut data: Vec<&str> = Vec::new();
    for line in STREAM.lines() {
        if line.is_empty() {
            if !data.is_empty() {
                let payload = data.join("\n");
                data.clear();
                if let Ok(value) = serde_json::from_str(&payload) {
                    events.push(value);
                }
            }
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("data: ") {
            data.push(value);
        }
    }
    events
}

#[test]
fn captured_stream_yields_only_mapped_events() {
    let events: Vec<RuntimeEvent> = frames().iter().filter_map(parse).collect();
    assert_eq!(
        events,
        vec![
            RuntimeEvent::SessionCreated {
                session_id: "ses_e382967f5ffe5zUw0VhEYnWu1m".to_string(),
                parent_id: None,
            },
            RuntimeEvent::TurnStarted {
                session_id: "ses_e382b0d8dffeOjsEVT3uTeZZ73".to_string(),
            },
            RuntimeEvent::TextEnded {
                session_id: "ses_e382b0d8dffeOjsEVT3uTeZZ73".to_string(),
                assistant_message_id: "msg_1c7d594520016Dgk9pHtISwlAW".to_string(),
                ordinal: 0,
                text: "OK".to_string(),
                time: 1794354031934,
            },
            RuntimeEvent::SessionIdle {
                session_id: "ses_e382b0d8dffeOjsEVT3uTeZZ73".to_string(),
            },
            RuntimeEvent::SessionDeleted {
                session_id: "ses_e382967f5ffe5zUw0VhEYnWu1m".to_string(),
            },
            RuntimeEvent::SessionDeleted {
                session_id: "ses_e382b0d8dffeOjsEVT3uTeZZ73".to_string(),
            },
        ]
    );
}

#[test]
fn envelope_keeps_the_location_directory() {
    let frame = frames()
        .into_iter()
        .find(|value| value["type"] == "session.text.ended")
        .expect("captured text.ended frame");
    let envelope = Envelope::parse(&frame).expect("envelope");
    assert_eq!(
        envelope.location.as_deref(),
        Some("C:\\Users\\example\\projects\\ntfy-proj")
    );
    assert_eq!(envelope.created, Some(1794354031934));
    assert_eq!(envelope.id, "evt_1c7d5ad3e002e9dEc2MElByhGH");
}

#[test]
fn frames_without_ids_or_types_are_not_envelopes() {
    use serde_json::json;
    assert!(Envelope::parse(&json!({"type": "session.text.ended", "data": {}})).is_none());
    assert!(Envelope::parse(&json!({"id": "evt_1", "data": {}})).is_none());
    assert!(Envelope::parse(&json!({"id": "", "type": "x", "data": {}})).is_none());
}
