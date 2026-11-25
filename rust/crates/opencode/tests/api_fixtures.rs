//! Decode tests against responses shaped like a running OpenCode server.
//!
//! The JSON under `tests/fixtures` keeps the recorded field shapes with
//! synthetic values: paths, titles, questions, and credentials are examples.
//! The point is to fail when a server upgrade renames a field.

use ntfy_relay_opencode::api::{
    decode_active_ids, decode_messages, decode_projects, decode_session, decode_sessions, BaseUrl,
    BasicAuth,
};

const PROJECTS: &[u8] = include_bytes!("fixtures/project.json");
const SESSIONS: &[u8] = include_bytes!("fixtures/sessions.json");
const ACTIVE: &[u8] = include_bytes!("fixtures/active.json");
const SERVICE: &[u8] = include_bytes!("fixtures/service.json");
const MESSAGES: &[u8] = include_bytes!("fixtures/messages.json");

#[test]
fn captured_project_list_keeps_duplicate_rows() {
    let projects = decode_projects(PROJECTS).unwrap();
    assert!(projects.len() >= 3);
    let first = &projects[0];
    assert_eq!(first.id, "1111111111111111111111111111111111111111");
    assert_eq!(
        first.canonical.as_deref(),
        Some("C:\\Users\\example\\projects\\ntfy-proj")
    );
    assert_eq!(first.time.active, 1794352811134);
    let duplicate = projects
        .iter()
        .filter(|project| project.canonical.as_deref() == first.canonical.as_deref())
        .count();
    assert_eq!(duplicate, 3);
}

#[test]
fn captured_sessions_decode_with_times_and_titles() {
    let sessions = decode_sessions(SESSIONS).unwrap();
    assert_eq!(sessions.len(), 3);
    assert_eq!(sessions[0].id, "ses_e383f93edffeAyUFn7nbhl1JgR");
    assert_eq!(sessions[0].title.as_deref(), Some("Example session alpha"));
    assert_eq!(sessions[0].time.updated, 1794352847309);
    assert_eq!(
        sessions[0].project_id.as_deref(),
        Some("1111111111111111111111111111111111111111")
    );
    assert_eq!(sessions[0].parent_id, None);
    assert!(sessions
        .windows(2)
        .all(|pair| pair[0].time.updated >= pair[1].time.updated));
}

#[test]
fn session_response_unwraps_the_data_envelope() {
    let bytes =
        br#"{"data":{"id":"ses_x","projectID":"p1","title":"t","time":{"created":1,"updated":2}}}"#;
    let session = decode_session(bytes).unwrap();
    assert_eq!(session.id, "ses_x");
    assert_eq!(session.time.updated, 2);
    assert_eq!(session.parent_id, None);
    assert!(decode_session(b"").is_err());
}

#[test]
fn child_sessions_carry_parent_id() {
    let bytes =
        br#"{"id":"ses_child","projectID":"p1","parentID":"ses_root","time":{"updated":3}}"#;
    let session = decode_session(bytes).unwrap();
    assert_eq!(session.parent_id.as_deref(), Some("ses_root"));
}

#[test]
fn active_map_keeps_only_running_ids_sorted() {
    let ids = decode_active_ids(ACTIVE).unwrap();
    assert_eq!(ids, vec!["ses_e383f93edffeAyUFn7nbhl1JgR".to_string()]);
    let mixed =
        br#"{"data":{"ses_b":{"type":"running"},"ses_a":{"type":"running"},"ses_c":{"type":"busy"}}}"#;
    assert_eq!(
        decode_active_ids(mixed).unwrap(),
        vec!["ses_a".to_string(), "ses_b".to_string()]
    );
}

#[test]
fn captured_message_page_carries_completion_times_and_a_cursor() {
    let page = decode_messages(MESSAGES).unwrap();
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[0].id, "msg_1d0a88b81001QDn3z5mXas0wVV");
    assert_eq!(page.messages[0].completed_at(), Some(1_794_502_074_641));
    let parts = page.messages[0].text_parts();
    assert_eq!(parts.len(), 1, "reasoning is not text");
    assert!(parts[0].starts_with("Handoff written and verified."));
    assert!(page.messages[1].text_parts().is_empty(), "tool only");
    assert!(page.next.is_some());
    assert!(page.messages[0].completed_at() > page.messages[1].completed_at());
}

#[test]
fn empty_and_wrapped_lists_are_tolerated() {
    assert!(decode_projects(b"").unwrap().is_empty());
    assert!(decode_sessions(b"").unwrap().is_empty());
    assert!(decode_sessions(br#"{"data":[]}"#).unwrap().is_empty());
}

#[test]
fn service_file_yields_basic_auth() {
    let auth = BasicAuth::from_service_json(SERVICE).unwrap();
    assert_eq!(auth.header(), "Basic b3BlbmNvZGU6Zml4dHVyZS1wYXNzd29yZA==");
    assert!(BasicAuth::from_service_json(br#"{"password":""}"#).is_err());
    assert!(BasicAuth::from_service_json(b"not-json").is_err());
}

#[test]
fn base_url_joins_encoded_paths() {
    let base = BaseUrl::parse("http://127.0.0.1:4096/").unwrap();
    assert_eq!(base.as_str(), "http://127.0.0.1:4096");
    assert_eq!(
        base.url("/api/session?project=abc&limit=20&order=desc&archived=false"),
        "http://127.0.0.1:4096/api/session?project=abc&limit=20&order=desc&archived=false"
    );
}
