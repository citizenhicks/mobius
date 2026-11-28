//! Command output and its streaming sink.

use std::sync::Arc;

use crate::Result;

/// Bounded output from a sandboxed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// The exit code.
    pub exit_code: i32,
    /// The stdout.
    pub stdout: String,
    /// The stdout truncated.
    pub stdout_truncated: bool,
    /// The stderr.
    pub stderr: String,
    /// The stderr truncated.
    pub stderr_truncated: bool,
}

/// One byte stream emitted by a sandboxed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStream {
    /// Selects the stdout case.
    Stdout,
    /// Selects the stderr case.
    Stderr,
}

/// Whether command execution has a foreground deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandMode {
    /// Selects the foreground case.
    Foreground,
    /// Selects the background case.
    Background,
}

/// Optional observer for bounded command output consumers.
#[derive(Clone, Default)]
pub struct CommandOutputSink {
    callback: Option<Arc<CommandOutputCallback>>,
}

type CommandOutputCallback = dyn Fn(CommandStream, &[u8]) + Send + Sync;

/// Authorizes one command at its process-launch boundary.
///
/// Returning without invoking the callback denies the launch. Implementations must invoke it
/// only while the authoritative authorization state is held.
pub type CommandAuthorization =
    Arc<dyn Fn(&mut dyn FnMut() -> Result<()>) -> Result<()> + Send + Sync>;

impl std::fmt::Debug for CommandOutputSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandOutputSink")
            .field("observed", &self.callback.is_some())
            .finish()
    }
}

impl CommandOutputSink {
    pub(crate) fn new(callback: impl Fn(CommandStream, &[u8]) + Send + Sync + 'static) -> Self {
        Self {
            callback: Some(Arc::new(callback)),
        }
    }

    /// Publishes one output chunk while the backend continues draining the stream.
    pub fn write(&self, stream: CommandStream, bytes: &[u8]) {
        if let Some(callback) = &self.callback {
            callback(stream, bytes);
        }
    }
}
