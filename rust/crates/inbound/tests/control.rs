//! Acceptance tests for the control table and parser, ported from
//! `tests/relay-control.test.mjs`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ntfy_relay_inbound::control::{
    builtin_control_table, default_control_path, load_control, parse_control, parse_native_command,
    render_ack, validate_control_table, Parsed, MAX_CONTROL_FILE_BYTES,
};
use serde_json::json;

fn temp_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("temp")
        .join("inbound-control-tests")
        .join(format!("{name}-{}-{unique}", std::process::id()));
    fs::create_dir_all(&path).expect("temp dir");
    path
}

fn parse(text: &str) -> Parsed {
    parse_control(text, &builtin_control_table())
}

#[test]
fn only_javascript_whitespace_separates_a_verb() {
    // U+0085 is whitespace in Rust but not in ECMAScript, so it stays in the
    // text and the line is a prompt, not a command.
    assert_eq!(parse("\u{85}/new"), Parsed::Prompt);
    assert_eq!(parse("/new\u{85}x"), Parsed::Prompt);
    assert_eq!(
        parse("/new\u{a0}x"),
        Parsed::Action {
            verb: "new".to_string(),
            arg: "x".to_string(),
            phrase: false,
        }
    );
}

#[test]
fn prefixes_resolve_the_same_verb_case_insensitively() {
    assert_eq!(
        parse("/NEW"),
        Parsed::Action {
            verb: "new".to_string(),
            arg: String::new(),
            phrase: false,
        }
    );
    assert_eq!(
        parse("hey RELAY new"),
        Parsed::Action {
            verb: "new".to_string(),
            arg: String::new(),
            phrase: true,
        }
    );
    assert_eq!(
        parse("  /new fix links  "),
        Parsed::Action {
            verb: "new".to_string(),
            arg: "fix links".to_string(),
            phrase: false,
        }
    );
    assert_eq!(parse("hey relay"), Parsed::Unknown { phrase: true });
    assert_eq!(parse("hey relay nope"), Parsed::Unknown { phrase: true });
    assert_eq!(parse("/nope"), Parsed::Prompt);
    assert_eq!(parse("/usr/bin/env is missing"), Parsed::Prompt);
    assert_eq!(parse("/steer hi"), Parsed::Prompt);
    assert_eq!(parse("hello"), Parsed::Prompt);
    assert_eq!(
        parse("/yes"),
        Parsed::Confirm {
            verb: "yes".to_string(),
        }
    );
    assert_eq!(parse("hey relayx"), Parsed::Prompt);
}

#[test]
fn aliases_resolve_to_the_canonical_verb() {
    let table = builtin_control_table();
    assert_eq!(
        parse_control("/start demo", &table),
        Parsed::Action {
            verb: "new".to_string(),
            arg: "demo".to_string(),
            phrase: false,
        }
    );
    assert_eq!(
        parse_control("hey relay start demo", &table),
        Parsed::Action {
            verb: "new".to_string(),
            arg: "demo".to_string(),
            phrase: true,
        }
    );
}

#[test]
fn native_commands_stay_outside_the_table() {
    let steer = parse_native_command("/steer  focus on the parser ").expect("native");
    assert_eq!(steer.verb, "steer");
    assert_eq!(steer.arg, "focus on the parser");
    assert!(parse_native_command("/allow").is_some());
    assert!(parse_native_command("hey relay steer x").is_none());
    assert!(parse_native_command("/new title").is_none());
}

#[test]
fn ack_templates_fill_known_fields_and_keep_unknown_ones() {
    let mut values = HashMap::new();
    values.insert("id".to_string(), "x".to_string());
    values.insert("project".to_string(), "p".to_string());
    assert_eq!(render_ack("new {id} in {project}", &values), "new x in p");
    let mut partial = HashMap::new();
    partial.insert("id".to_string(), "x".to_string());
    assert_eq!(
        render_ack("new {id} in {project}", &partial),
        "new x in {project}"
    );
    assert_eq!(render_ack("{a b} {unclosed", &partial), "{a b} {unclosed");
}

#[test]
fn config_rejects_bad_versions_handlers_capabilities_and_collisions() {
    let base = json!({
        "version": 1,
        "prefixes": ["/"],
        "caseSensitive": false,
        "actions": { "x": { "handler": "relay.help" } }
    });

    let error = |value| validate_control_table(&value).unwrap_err().to_string();
    assert!(error(json!({ "version": 2, "prefixes": ["/"], "caseSensitive": false, "actions": { "x": { "handler": "relay.help" } } }))
        .contains("version must be 1"));
    assert!(error(json!({ "version": 1, "prefixes": [], "caseSensitive": false, "actions": { "x": { "handler": "relay.help" } } }))
        .contains("prefixes"));
    assert!(error(json!({ "version": 1, "prefixes": ["/"], "caseSensitive": false, "actions": { "x": { "handler": "shell.run" } } }))
        .contains("handler: must be one of"));
    assert!(error(json!({ "version": 1, "prefixes": ["/"], "caseSensitive": false, "actions": { "x": { "handler": "relay.help", "requires": ["camera"] } } }))
        .contains("unknown capability camera"));
    assert!(error(json!({ "version": 1, "prefixes": ["/"], "caseSensitive": false, "actions": { "yes": { "handler": "relay.help" } } }))
        .contains("reserved"));
    assert!(error(json!({
        "version": 1,
        "prefixes": ["/"],
        "caseSensitive": false,
        "actions": {
            "new": { "handler": "relay.help" },
            "x": { "handler": "relay.help", "aliases": ["new"] }
        }
    }))
    .contains("collides with another verb or alias"));
    assert!(error(json!({ "version": 1, "prefixes": ["/"], "caseSensitive": false, "actions": { "x": { "handler": "relay.help", "confirm": "yes" } } }))
        .contains("confirm: must be a boolean"));
    assert!(error(json!({ "version": 1, "prefixes": ["/"], "caseSensitive": false, "actions": { "x": { "handler": "relay.help", "extra": 1 } } }))
        .contains("unknown field"));

    assert!(validate_control_table(&base).is_ok());
}

#[test]
fn builtin_table_has_the_documented_actions() {
    let table = builtin_control_table();
    assert_eq!(table.version, 1);
    assert_eq!(
        table.prefixes,
        vec!["/".to_string(), "hey relay".to_string()]
    );
    assert_eq!(table.len(), 6);

    let new = table.action("new").expect("new");
    assert_eq!(new.handler.to_string(), "session.create");
    assert!(new.target);
    assert_eq!(new.cooldown_ms, 5000);
    assert_eq!(new.ack.as_deref(), Some("new session {id} in {project}"));

    assert_eq!(
        table.action("status").expect("status").handler.to_string(),
        "relay.status"
    );
    assert!(table.action("start").is_some());
}

#[test]
fn missing_file_falls_back_to_the_builtin_table() {
    let dir = temp_dir("missing");
    let file = dir.join("ntfy-relay-control.json");
    let loaded = load_control(&file).expect("load");
    assert!(loaded.missing);
    assert!(!file.exists());
    assert_eq!(
        loaded.table.action("new").expect("new").handler.to_string(),
        "session.create"
    );
    fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn invalid_file_fails_with_path_and_reason() {
    let dir = temp_dir("invalid");
    let file = dir.join("ntfy-relay-control.json");

    fs::write(&file, "{nope").expect("write");
    let error = load_control(&file).unwrap_err().to_string();
    assert!(error.contains("invalid JSON"), "{error}");
    assert!(error.contains("ntfy-relay-control.json"), "{error}");

    fs::write(
        &file,
        r#"{"version":1,"prefixes":["/"],"caseSensitive":false,"actions":{"x":{"handler":"nope"}}}"#,
    )
    .expect("write");
    let error = load_control(&file).unwrap_err().to_string();
    assert!(error.contains("handler: must be one of"), "{error}");

    fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn file_is_size_capped_and_must_be_regular() {
    let dir = temp_dir("size");
    let file = dir.join("ntfy-relay-control.json");
    let oversized = "x".repeat(MAX_CONTROL_FILE_BYTES as usize + 1);
    fs::write(&file, oversized).expect("write");
    let error = load_control(&file).unwrap_err().to_string();
    assert!(error.contains("exceeds 65536 bytes"), "{error}");

    let error = load_control(&dir).unwrap_err().to_string();
    assert!(error.contains("not a regular file"), "{error}");

    fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn default_path_is_the_documented_location() {
    let root = Path::new(r"C:\relay");
    let expected = root.join("ntfy-relay-control.json");
    assert_eq!(default_control_path(root), expected);
}
