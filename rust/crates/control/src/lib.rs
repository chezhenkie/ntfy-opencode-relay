//! Shared types and policy for the loopback control endpoint.
//!
//! The contract is `docs/specsheet/12-control-path.md`. The daemon and
//! `ntfy-admin` both link this crate; it adds no binary.

pub mod client;
pub mod policy;
pub mod process;
pub mod runtime;
pub mod server;
pub mod types;

#[cfg(test)]
pub mod test_support;
