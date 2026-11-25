//! `ntfy-auth`: bootstraps the one ntfy principal, its wildcard row, and its
//! token.
//!
//! The crate owns `ntfy-auth.json` in the relay install root and runs only the
//! server CLI inside the running container. It never edits `auth.db`, never
//! creates a second container, and never prints a token. Spec
//! `docs/specsheet/11-auth-bootstrap.md`.

pub mod auth;
pub mod config;
pub mod runner;

#[cfg(test)]
pub(crate) mod test_support;

pub use auth::{AuthEngine, AuthError, Report, StatusReport, TokenState};
pub use config::{AuthConfig, ConfigError, Permission};
pub use runner::{CommandOutput, CommandRunner, SystemRunner};
