//! Local OpenCode client and `SessionApi` implementation.
//!
//! The Node relay shells out to `opencode api ...`; this crate talks to the
//! same HTTP routes directly (docs/specsheet `03-rust-relay-merge.md`, section 6),
//! so no child process and no PATH assumption is needed.
//!
//! - `api` owns the wire: base URL, basic auth, session and project calls.
//! - `event` owns the native event stream types, parsed like the plugin.
//! - `adapter` implements `core::adapter::SessionApi`; `inbound` never depends
//!   on this crate and stays reusable for the pi and omp seams.

pub mod adapter;
pub mod api;
pub mod event;
pub mod event_stream;
