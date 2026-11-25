//! Inbound message handling: filter, dedupe, queue, routing, prompting,
//! permission replies, inbox prune, and the control table.
//!
//! The SSE reader and the daemon wiring live in `app`; this crate owns the
//! semantics the inbound suite pins, and talks to a harness only through
//! `core::adapter` traits.

pub mod control;
pub mod filter;
pub mod form_reply;
pub mod permission;
pub mod prompting;
pub mod prune;
pub mod queue;
pub mod rate;
pub mod routing;
pub mod session;
