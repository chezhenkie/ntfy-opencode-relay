//! Outbound phone notifications from the OpenCode event stream.
//!
//! Replaces the patched plugin's notification half: one `GET /api/event`
//! subscription feeds a bounded channel, a worker assembles per-turn text,
//! scans newly created files at the terminal, and maps permission asks and
//! question forms to ntfy pushes. Topic derivation and path scope come from
//! the session's project canonical, bounded by `projectRoots`.
//!
//! The MCP `send_file` tool is a library call here; `ntfy-relay mcp` is the
//! thin stdio protocol layer in `rust/crates/app`.

pub mod files;
pub mod format;
pub mod lane;
pub mod log;
pub mod permission;
pub mod publish;
pub mod runner;
pub mod scope;
pub mod seen;
pub mod topic;
pub mod turn;
