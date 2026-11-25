//! Control: the JSON action table, its parser, and the command engine.
//!
//! Mirrors `relay-control.mjs`. The table is data; the handler registry is
//! closed; a missing file selects the built-in table and nothing is written.

pub mod engine;
pub mod load;
pub mod parse;
pub mod table;

pub use engine::{Control, Decision, Run, Status};
pub use load::{default_control_path, load_control, ControlLoadError, LoadedControl};
pub use ntfy_relay_core::adapter::Capability;
pub use parse::{
    parse_control, parse_native_command, render_ack, session_title, short_session_id,
    NativeCommand, Parsed, ParsedNative, NATIVE_COMMANDS,
};
pub use table::{
    builtin_control_table, validate_control_table, Action, ControlConfigError, ControlError,
    ControlTable, Handler, CONFIRM_EXPIRED_GRACE_MS, CONFIRM_WINDOW_MS, CONTROL_VERSION,
    MAX_ACTIONS, MAX_ALIASES, MAX_CONTROL_FILE_BYTES, MAX_COOLDOWN_MS, MAX_PREFIXES,
};
