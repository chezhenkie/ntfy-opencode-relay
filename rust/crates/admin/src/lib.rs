//! `ntfy-admin`: the console front end for the relay control endpoint.
//!
//! The contract is `docs/specsheet/12-control-path.md`. The CLI talks to the
//! loopback endpoint only; it never reads the relay config or the ntfy token.

pub mod cli;
pub mod endpoint;
pub mod run;
