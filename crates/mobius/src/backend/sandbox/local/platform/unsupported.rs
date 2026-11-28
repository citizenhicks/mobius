use tokio::process::Command;

use super::super::Invocation;
use super::super::LocalSandbox;
use super::super::NetworkAccess;
use super::super::WorkspaceAccess;
use crate::Error;
use crate::Result;

pub(crate) fn sandboxed_command(
    _sandbox: &LocalSandbox,
    _invocation: &Invocation<'_>,
    _network_access: NetworkAccess,
    _workspace_access: WorkspaceAccess,
) -> Result<Command> {
    Err(Error::Sandbox(
        "local code execution requires Linux or macOS".into(),
    ))
}

pub(crate) fn protected_full_access_command(
    _sandbox: &LocalSandbox,
    _invocation: &Invocation<'_>,
) -> Result<Command> {
    Err(Error::Sandbox(
        "protected full-access execution requires Linux or macOS".into(),
    ))
}
