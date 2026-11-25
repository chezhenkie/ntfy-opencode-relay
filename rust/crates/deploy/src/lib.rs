//! `ntfy-deploy`: switches the ntfy container between proxy, VPN, and LAN
//! modes.
//!
//! The crate owns the deployment config, the container environment, and the
//! Windows portproxy. It never touches the relay daemon, its config, or its
//! token, and it reads no OpenCode file. Spec `docs/specsheet/10-deploy-mode.md`.

pub mod config;
pub mod deploy;
pub mod install;
pub mod mode;
pub mod platform;
pub mod runner;
pub mod status;

#[cfg(test)]
pub(crate) mod test_support;

pub use config::{BaseUrl, ConfigError, DeployConfig, Mode};
pub use deploy::{DeployError, Deployer, HealthProbe, HealthRetry, Report, TcpHealth};
pub use install::{install, remove, InstallPaths, InstallReport};
pub use mode::{Choice, TailscaleStatus};
pub use platform::Platform;
pub use runner::{CommandOutput, CommandRunner, SystemRunner};
