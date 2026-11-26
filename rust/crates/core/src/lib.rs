//! Shared domain types for the ntfy relay.

pub mod adapter;
pub mod asks;
pub mod clock;
pub mod config;
pub mod forms;
pub mod fsutil;
pub mod link_safe;
pub mod net;
pub mod state;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod text;
pub mod tlds;
pub mod topic;
