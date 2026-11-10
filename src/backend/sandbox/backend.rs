//! The execution backend a sandbox wraps.

use crate::SessionId;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use super::CommandAuthorization;
use super::CommandMode;
use super::CommandOutput;
use super::CommandOutputSink;
use super::NetworkAccess;
use super::SandboxMode;
use super::WorkerCommand;
use super::WorkerProcess;
use crate::BoxFuture;
use crate::Error;
use crate::Result;

/// Implements one sandbox execution environment.
///
/// Returned futures are [`Send`] and may be dropped during execution. Backends
/// own cancellation cleanup for the processes and resources they launch;
/// dropping a future must not leave unmanaged commands running. [`Sandbox`](super::Sandbox)
/// owns approval and background-command tracking, not arbitrary backend cleanup.
pub trait SandboxBackend: Send + Sync {
    /// Checks whether this execution environment is accepting work.
    /// # Errors
    ///
    /// Returns an error while the backend has suspended execution.
    fn check_execution(&self) -> Result<()> {
        Ok(())
    }

    /// Creates an independent temporary area and execution lifetime for a child agent.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    fn isolated_execution(&self) -> Result<Arc<dyn SandboxBackend>> {
        Err(Error::Sandbox(
            "backend does not support isolated child execution".into(),
        ))
    }

    /// Reports the private temporary path visible to command and file tools.
    fn temporary_directory(&self) -> Option<PathBuf> {
        None
    }

    /// Launches a persistent framed runtime inside this backend's execution boundary.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    fn start_worker(
        &self,
        _command: &WorkerCommand,
        _sandbox_mode: SandboxMode,
        _network_access: NetworkAccess,
    ) -> Result<WorkerProcess> {
        Err(Error::Sandbox(
            "persistent execution is unavailable on this backend".into(),
        ))
    }

    /// Opens an authorized host-service channel for one worker evaluation.
    /// Dropping the channel ends its authority; requests are never replayed.
    fn worker_connection<'a>(
        &'a self,
        _session_id: &'a SessionId,
        _sandbox_mode: SandboxMode,
        _network_access: NetworkAccess,
    ) -> BoxFuture<'a, Result<tokio::io::DuplexStream>> {
        Box::pin(async {
            Err(Error::Sandbox(
                "native desktop control is unavailable".into(),
            ))
        })
    }

    /// Finds the gateway-owned browser page assigned to this execution, if available.
    /// # Errors
    ///
    /// Returns an error if the backend cannot assign a page or has suspended execution.
    fn desktop_browser_page<'a>(
        &'a self,
        _session_id: &'a SessionId,
    ) -> BoxFuture<'a, Result<Option<DesktopBrowserPage>>> {
        Box::pin(async { Ok(None) })
    }

    /// Reads a UTF-8 file under the requested isolation.
    fn read<'a>(
        &'a self,
        path: &'a str,
        sandbox_mode: SandboxMode,
    ) -> BoxFuture<'a, Result<String>>;

    /// Reads one binary file through a single bounded open handle under the requested isolation.
    fn read_bytes<'a>(
        &'a self,
        path: &'a str,
        max_bytes: usize,
        sandbox_mode: SandboxMode,
    ) -> BoxFuture<'a, Result<Vec<u8>>>;

    /// Writes a UTF-8 file under the requested isolation.
    fn write<'a>(
        &'a self,
        path: &'a str,
        content: &'a str,
        sandbox_mode: SandboxMode,
    ) -> BoxFuture<'a, Result<()>>;

    /// Runs a shell command and forwards drained output under the requested isolation.
    fn execute<'a>(
        &'a self,
        command: &'a str,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
        mode: CommandMode,
        output: CommandOutputSink,
    ) -> BoxFuture<'a, Result<CommandOutput>>;

    /// Runs a shell command only when authorization launches it atomically.
    ///
    /// Backends without an atomic launch boundary fail closed.
    fn execute_authorized<'a>(
        &'a self,
        _command: &'a str,
        _sandbox_mode: SandboxMode,
        _network_access: NetworkAccess,
        _mode: CommandMode,
        _output: CommandOutputSink,
        _authorization: &'a CommandAuthorization,
    ) -> BoxFuture<'a, Result<Option<CommandOutput>>> {
        Box::pin(async { Ok(None) })
    }
}

/// Fills a test backend's required file methods, and `execute` when asked, with bodies a
/// test never reaches.
#[cfg(test)]
macro_rules! unreachable_backend_methods {
    () => {
        fn read<'a>(
            &'a self,
            _: &'a str,
            _: $crate::backend::sandbox::SandboxMode,
        ) -> $crate::BoxFuture<'a, $crate::Result<String>> {
            Box::pin(async { unreachable!() })
        }

        fn read_bytes<'a>(
            &'a self,
            _: &'a str,
            _: usize,
            _: $crate::backend::sandbox::SandboxMode,
        ) -> $crate::BoxFuture<'a, $crate::Result<Vec<u8>>> {
            Box::pin(async { unreachable!() })
        }

        fn write<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: $crate::backend::sandbox::SandboxMode,
        ) -> $crate::BoxFuture<'a, $crate::Result<()>> {
            Box::pin(async { unreachable!() })
        }
    };
    (execute) => {
        $crate::backend::sandbox::unreachable_backend_methods!();

        fn execute<'a>(
            &'a self,
            _: &'a str,
            _: $crate::backend::sandbox::SandboxMode,
            _: $crate::backend::sandbox::NetworkAccess,
            _: $crate::backend::sandbox::CommandMode,
            _: $crate::backend::sandbox::CommandOutputSink,
        ) -> $crate::BoxFuture<'a, $crate::Result<$crate::backend::sandbox::CommandOutput>> {
            Box::pin(async { unreachable!() })
        }
    };
}
#[cfg(test)]
pub(crate) use unreachable_backend_methods;

/// A browser owned outside the worker and its assigned page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopBrowserPage {
    /// A loopback browser endpoint or a scoped local Unix page endpoint.
    pub endpoint: String,
    /// The exact remote page target; local Unix endpoints expose one page only.
    pub target_id: Option<String>,
}
