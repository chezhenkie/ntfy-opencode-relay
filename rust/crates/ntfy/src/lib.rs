//! ntfy wire client: publish styles, SSE parsing, attachment fetch, headers.
//!
//! Mirrors `publishNtfy` in `ntfy-inbox-relay.mjs` (header style) and
//! `createNtfyClient` in the plugin dist (JSON style and file PUT). The crate
//! owns the wire, not the policy: no topic derivation, no link safety, and no
//! caller budgets. The only caps here are the plugin's title and message
//! limits, because they are part of the serialized request.

pub mod attach;
pub mod header;
pub mod http;
pub mod publish;
pub mod sse;
pub mod stream;
