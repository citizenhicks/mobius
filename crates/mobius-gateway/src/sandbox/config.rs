//! Operator-selected command execution policy.

use crate::{Error, Result};
use mobius::middleware::subagents::SubagentCeilings;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Host command policy independent of Bot permissions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ExecutionFile", into = "ExecutionFile")]
pub struct ExecutionConfig {
    /// Deadline for each agent command.
    pub command_timeout_seconds: u64,
    /// Optional absolute location of a shell accepting `-c` scripts.
    pub shell_executable: Option<PathBuf>,
    /// Optional absolute location of Bubblewrap on Linux.
    pub bubblewrap_executable: Option<PathBuf>,
    /// Linux procfs policy; empty procfs requires host-provided PID isolation.
    pub procfs_mode: mobius::backend::sandbox::ProcfsMode,
    /// Configurable fallback estimate when measured token usage is unavailable.
    pub bytes_per_token: f64,
    /// Host ceilings for each Bot's subagent tree, validated when read.
    pub subagent_ceilings: SubagentCeilings,
}

/// The flat on-disk shape of [`ExecutionConfig`].
#[derive(Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExecutionFile {
    command_timeout_seconds: u64,
    shell_executable: Option<PathBuf>,
    bubblewrap_executable: Option<PathBuf>,
    procfs_mode: mobius::backend::sandbox::ProcfsMode,
    bytes_per_token: f64,
    subagent_max_depth: u8,
    subagent_max_concurrency: usize,
    subagent_max_agents: usize,
}

impl Default for ExecutionFile {
    fn default() -> Self {
        ExecutionConfig::default().into()
    }
}

impl TryFrom<ExecutionFile> for ExecutionConfig {
    type Error = mobius::Error;

    fn try_from(file: ExecutionFile) -> mobius::Result<Self> {
        Ok(Self {
            command_timeout_seconds: file.command_timeout_seconds,
            shell_executable: file.shell_executable,
            bubblewrap_executable: file.bubblewrap_executable,
            procfs_mode: file.procfs_mode,
            bytes_per_token: file.bytes_per_token,
            subagent_ceilings: SubagentCeilings::new(
                file.subagent_max_depth,
                file.subagent_max_concurrency,
                file.subagent_max_agents,
            )?,
        })
    }
}

impl From<ExecutionConfig> for ExecutionFile {
    fn from(config: ExecutionConfig) -> Self {
        Self {
            command_timeout_seconds: config.command_timeout_seconds,
            shell_executable: config.shell_executable,
            bubblewrap_executable: config.bubblewrap_executable,
            procfs_mode: config.procfs_mode,
            bytes_per_token: config.bytes_per_token,
            subagent_max_depth: config.subagent_ceilings.max_depth(),
            subagent_max_concurrency: config.subagent_ceilings.max_concurrency(),
            subagent_max_agents: config.subagent_ceilings.max_agents(),
        }
    }
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            command_timeout_seconds: mobius::backend::sandbox::default_command_timeout_seconds(),
            shell_executable: None,
            bubblewrap_executable: None,
            procfs_mode: mobius::backend::sandbox::ProcfsMode::default(),
            bytes_per_token: mobius::middleware::TokenEstimate::default().bytes_per_token(),
            subagent_ceilings: SubagentCeilings::default(),
        }
    }
}
impl ExecutionConfig {
    /// Validates execution policy before building a sandbox.
    /// # Errors
    /// Returns an error for invalid durations, paths or token estimates.
    pub fn validate(&self) -> Result<()> {
        crate::config::bounded(
            "execution.command_timeout_seconds",
            self.command_timeout_seconds,
            1..=31_536_000,
        )?;
        for path in [&self.shell_executable, &self.bubblewrap_executable]
            .into_iter()
            .flatten()
        {
            if !path.is_absolute() || !path.is_file() {
                return Err(Error::Config(
                    "execution executable must be an existing absolute file".into(),
                ));
            }
        }
        mobius::middleware::TokenEstimate::new(self.bytes_per_token)?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_execution_fields_reuse_core_defaults_without_recursive_initialization() {
        let policy: ExecutionConfig = toml::from_str("").expect("defaulted execution policy");
        assert_eq!(policy, ExecutionConfig::default());
        assert_eq!(
            policy.command_timeout_seconds,
            mobius::backend::sandbox::default_command_timeout_seconds()
        );
        assert_eq!(policy.subagent_ceilings, SubagentCeilings::default());
        policy.validate().expect("valid defaults");
    }

    #[test]
    fn procfs_policy_is_explicit_and_rejects_unknown_modes() {
        use mobius::backend::sandbox::ProcfsMode;

        assert_eq!(ExecutionConfig::default().procfs_mode, ProcfsMode::Private);
        let policy: ExecutionConfig =
            toml::from_str("procfs_mode = 'empty'").expect("empty procfs");
        assert_eq!(policy.procfs_mode, ProcfsMode::Empty);
        assert!(toml::from_str::<ExecutionConfig>("procfs_mode = 'automatic'").is_err());
    }

    #[test]
    fn trusted_operator_subagent_ceilings_are_validated_when_read() {
        let policy: ExecutionConfig = toml::from_str(
            "subagent_max_depth=32\nsubagent_max_concurrency=128\nsubagent_max_agents=512",
        )
        .expect("operator settings");
        assert_eq!(
            policy.subagent_ceilings,
            SubagentCeilings::new(32, 128, 512).expect("larger trusted ceilings")
        );
        assert!(
            toml::to_string(&policy)
                .expect("serialize policy")
                .contains("subagent_max_agents = 512")
        );
        for invalid in [
            "subagent_max_depth=32\nsubagent_max_concurrency=128\nsubagent_max_agents=127",
            "subagent_max_depth=0",
        ] {
            assert!(toml::from_str::<ExecutionConfig>(invalid).is_err());
        }
    }
}
