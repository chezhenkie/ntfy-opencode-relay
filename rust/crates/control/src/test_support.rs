//! Test-only scratch directories.
//!
//! The guard lives in `core::test_support` so every crate uses one rule: a
//! unique path under the system temp, removed on drop.

pub use ntfy_relay_core::test_support::Scratch as TempDir;
