//! Operator-selected command execution policy.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Host command policy independent of Bot permissions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionConfig {
    /// Deadline for each agent command.
    pub command_timeout_seconds: u64,
    /// Optional absolute location of a shell accepting `-c` scripts.
    pub shell_executable: Option<PathBuf>,
    /// Optional absolute location of Bubblewrap on Linux.
    pub bubblewrap_executable: Option<PathBuf>,
    /// Linux procfs policy; empty procfs requires host-provided PID isolation.
    pub procfs_mode: mobius::backend::sandbox::ProcfsMode,
    /// Additional host environment names allowed into isolated commands.
    /// Credential removal always takes precedence.
    pub allow_environment: Vec<String>,
    /// Configurable fallback estimate when measured token usage is unavailable.
    pub bytes_per_token: f64,
    /// Host ceiling for each Bot's subagent nesting depth.
    pub subagent_max_depth: u8,
    /// Host ceiling for concurrent agents, including the root.
    pub subagent_max_concurrency: usize,
    /// Host ceiling for retained agents, including the root.
    pub subagent_max_agents: usize,
}
impl Default for ExecutionConfig {
    fn default() -> Self {
        let ceilings = mobius::middleware::subagents::SubagentCeilings::default();
        Self {
            command_timeout_seconds: mobius::backend::sandbox::default_command_timeout_seconds(),
            shell_executable: None,
            bubblewrap_executable: None,
            procfs_mode: mobius::backend::sandbox::ProcfsMode::default(),
            allow_environment: Vec::new(),
            bytes_per_token: mobius::middleware::TokenEstimate::default().bytes_per_token(),
            subagent_max_depth: ceilings.max_depth(),
            subagent_max_concurrency: ceilings.max_concurrency(),
            subagent_max_agents: ceilings.max_agents(),
        }
    }
}
impl ExecutionConfig {
    /// Validates execution policy before building a sandbox.
    /// # Errors
    /// Returns an error for invalid durations, paths, variable names, or token estimates.
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
        if self.allow_environment.len() > 256
            || self.allow_environment.iter().any(|name| {
                !mobius::identifier::valid_ascii_identifier(
                    name,
                    256,
                    mobius::identifier::AsciiCase::Any,
                    b"_",
                ) || name.as_bytes().first().is_some_and(u8::is_ascii_digit)
            })
        {
            return Err(Error::Config(
                "execution.allow_environment contains an invalid variable name".into(),
            ));
        }
        mobius::middleware::TokenEstimate::new(self.bytes_per_token)?;
        self.subagent_ceilings()?;
        Ok(())
    }

    /// Returns validated host ceilings independent of frontend-writable Bot settings.
    /// # Errors
    /// Returns an error for invalid depth, concurrency, or agent relationships.
    pub fn subagent_ceilings(&self) -> Result<mobius::middleware::subagents::SubagentCeilings> {
        Ok(mobius::middleware::subagents::SubagentCeilings::new(
            self.subagent_max_depth,
            self.subagent_max_concurrency,
            self.subagent_max_agents,
        )?)
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
        assert_eq!(
            policy.subagent_ceilings().expect("ceilings"),
            mobius::middleware::subagents::SubagentCeilings::default()
        );
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
    fn trusted_operator_subagent_ceiling_relationships_are_validated() {
        let mut policy: ExecutionConfig = toml::from_str(
            "subagent_max_depth=32\nsubagent_max_concurrency=128\nsubagent_max_agents=512",
        )
        .expect("operator settings");
        policy.validate().expect("larger trusted ceilings");
        policy.subagent_max_agents = 127;
        assert!(policy.validate().is_err());
        policy.subagent_max_agents = 512;
        policy.subagent_max_depth = 0;
        assert!(policy.validate().is_err());
    }
}
