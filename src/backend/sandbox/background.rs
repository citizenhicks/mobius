use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::task::JoinHandle;
use uuid::Uuid;

use super::{
    CommandMode, CommandOutput, CommandOutputSink, CommandStream, NetworkAccess, SandboxBackend,
    SandboxMode,
};
use crate::{Error, Result};

const MAX_BACKGROUND_COMMANDS: usize = 4;
const MAX_POLL_OUTPUT_BYTES: usize = 6_000;
const MAX_ERROR_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackgroundCommandStatus {
    Running,
    Exited,
    Failed,
    Stopped,
}

impl BackgroundCommandStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

pub(crate) struct BackgroundCommandPoll {
    pub(crate) status: BackgroundCommandStatus,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) truncated: bool,
    pub(crate) error: Option<String>,
}

#[derive(Default)]
pub(super) struct BackgroundCommands {
    entries: Mutex<BTreeMap<String, Entry>>,
}

struct Entry {
    owner: String,
    output: Arc<Mutex<BufferedOutput>>,
    task: JoinHandle<Result<CommandOutput>>,
}

#[derive(Default)]
struct BufferedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    saw_output: bool,
}

impl BackgroundCommands {
    pub(super) fn has_owner(&self, owner: &str) -> Result<bool> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| state_error())?
            .values()
            .any(|entry| entry.owner == owner))
    }

    pub(super) fn start(
        &self,
        owner: &str,
        backend: Arc<dyn SandboxBackend>,
        command: String,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
    ) -> Result<String> {
        let mut entries = self.entries.lock().map_err(|_| state_error())?;
        if entries.len() >= MAX_BACKGROUND_COMMANDS {
            return Err(Error::Sandbox(format!(
                "background command limit {MAX_BACKGROUND_COMMANDS} reached"
            )));
        }
        let id = loop {
            let candidate = Uuid::new_v4().to_string();
            if !entries.contains_key(&candidate) {
                break candidate;
            }
        };
        let output = Arc::new(Mutex::new(BufferedOutput::default()));
        let streamed = Arc::clone(&output);
        let sink = CommandOutputSink::new(move |stream, bytes| {
            if let Ok(mut output) = streamed.lock() {
                output.push(stream, bytes);
            }
        });
        let task = tokio::spawn(async move {
            backend
                .execute(
                    &command,
                    sandbox_mode,
                    network_access,
                    CommandMode::Background,
                    sink,
                )
                .await
        });
        entries.insert(
            id.clone(),
            Entry {
                owner: owner.into(),
                output,
                task,
            },
        );
        Ok(id)
    }

    pub(super) async fn poll(&self, owner: &str, id: &str) -> Result<BackgroundCommandPoll> {
        let entry = {
            let mut entries = self.entries.lock().map_err(|_| state_error())?;
            let entry = entries.get(id).ok_or_else(|| unknown(id))?;
            ensure_owner(entry, owner, id)?;
            if !entry.task.is_finished() {
                return poll_running(&entry.output);
            }
            entries.remove(id).ok_or_else(|| unknown(id))?
        };
        completed(entry).await
    }

    pub(super) async fn stop(&self, owner: &str, id: &str) -> Result<BackgroundCommandPoll> {
        let entry = {
            let mut entries = self.entries.lock().map_err(|_| state_error())?;
            let entry = entries.get(id).ok_or_else(|| unknown(id))?;
            ensure_owner(entry, owner, id)?;
            entries.remove(id).ok_or_else(|| unknown(id))?
        };
        entry.task.abort();
        let result = entry.task.await;
        if result
            .as_ref()
            .is_err_and(tokio::task::JoinError::is_cancelled)
        {
            return stopped(&entry.output);
        }
        finish(&entry.output, result)
    }

    pub(super) async fn shutdown(&self, owner: &str) -> Result<()> {
        let entries = {
            let mut entries = self.entries.lock().map_err(|_| state_error())?;
            let ids = entries
                .iter()
                .filter(|(_, entry)| entry.owner == owner)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| entries.remove(&id))
                .collect::<Vec<_>>()
        };
        for entry in &entries {
            entry.task.abort();
        }
        for entry in entries {
            let _ = entry.task.await;
        }
        Ok(())
    }
}

impl Drop for BackgroundCommands {
    fn drop(&mut self) {
        if let Ok(entries) = self.entries.get_mut() {
            for entry in entries.values() {
                entry.task.abort();
            }
        }
    }
}

impl BufferedOutput {
    fn push(&mut self, stream: CommandStream, bytes: &[u8]) {
        self.saw_output |= !bytes.is_empty();
        let remaining = MAX_POLL_OUTPUT_BYTES
            .saturating_sub(self.stdout.len().saturating_add(self.stderr.len()));
        let kept = bytes.len().min(remaining);
        match stream {
            CommandStream::Stdout => self.stdout.extend_from_slice(&bytes[..kept]),
            CommandStream::Stderr => self.stderr.extend_from_slice(&bytes[..kept]),
        }
        self.truncated |= kept < bytes.len();
    }

    fn take(&mut self) -> TakenOutput {
        TakenOutput {
            stdout: String::from_utf8_lossy(&std::mem::take(&mut self.stdout)).into_owned(),
            stderr: String::from_utf8_lossy(&std::mem::take(&mut self.stderr)).into_owned(),
            truncated: std::mem::take(&mut self.truncated),
            saw_output: self.saw_output,
        }
    }
}

struct TakenOutput {
    stdout: String,
    stderr: String,
    truncated: bool,
    saw_output: bool,
}

fn poll_running(output: &Arc<Mutex<BufferedOutput>>) -> Result<BackgroundCommandPoll> {
    let output = take_output(output)?;
    Ok(BackgroundCommandPoll {
        status: BackgroundCommandStatus::Running,
        exit_code: None,
        stdout: output.stdout,
        stderr: output.stderr,
        truncated: output.truncated,
        error: None,
    })
}

async fn completed(entry: Entry) -> Result<BackgroundCommandPoll> {
    let result = entry.task.await;
    finish(&entry.output, result)
}

fn stopped(output: &Arc<Mutex<BufferedOutput>>) -> Result<BackgroundCommandPoll> {
    let output = take_output(output)?;
    Ok(BackgroundCommandPoll {
        status: BackgroundCommandStatus::Stopped,
        exit_code: None,
        stdout: output.stdout,
        stderr: output.stderr,
        truncated: output.truncated,
        error: None,
    })
}

fn finish(
    output: &Arc<Mutex<BufferedOutput>>,
    result: std::result::Result<Result<CommandOutput>, tokio::task::JoinError>,
) -> Result<BackgroundCommandPoll> {
    let mut output = take_output(output)?;
    match result {
        Ok(Ok(command)) => {
            if !output.saw_output {
                output.stdout = command.stdout;
                output.stderr = command.stderr;
            }
            Ok(BackgroundCommandPoll {
                status: BackgroundCommandStatus::Exited,
                exit_code: Some(command.exit_code),
                stdout: output.stdout,
                stderr: output.stderr,
                truncated: output.truncated,
                error: None,
            })
        }
        Ok(Err(error)) => {
            let error = error.to_string();
            let truncated = error.len() > MAX_ERROR_BYTES;
            Ok(BackgroundCommandPoll {
                status: BackgroundCommandStatus::Failed,
                exit_code: None,
                stdout: output.stdout,
                stderr: output.stderr,
                truncated: output.truncated || truncated,
                error: Some(crate::truncate_utf8(&error, MAX_ERROR_BYTES).into()),
            })
        }
        Err(error) => Ok(BackgroundCommandPoll {
            status: if error.is_cancelled() {
                BackgroundCommandStatus::Stopped
            } else {
                BackgroundCommandStatus::Failed
            },
            exit_code: None,
            stdout: output.stdout,
            stderr: output.stderr,
            truncated: output.truncated,
            error: (!error.is_cancelled()).then(|| "background command task failed".into()),
        }),
    }
}

fn take_output(output: &Arc<Mutex<BufferedOutput>>) -> Result<TakenOutput> {
    Ok(output.lock().map_err(|_| state_error())?.take())
}

fn ensure_owner(entry: &Entry, owner: &str, id: &str) -> Result<()> {
    if entry.owner != owner {
        return Err(unknown(id));
    }
    Ok(())
}

fn unknown(id: &str) -> Error {
    Error::Unknown(format!("background command `{id}`"))
}

fn state_error() -> Error {
    Error::Stopped("background command state lock poisoned".into())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::sync::Notify;

    use super::*;
    use crate::BoxFuture;

    struct StreamingBackend {
        started: Arc<Notify>,
        release: Arc<Notify>,
        expected_sandbox_mode: SandboxMode,
    }

    impl SandboxBackend for StreamingBackend {
        fn read<'a>(&'a self, _path: &'a str) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }

        fn read_bytes<'a>(
            &'a self,
            _path: &'a str,
            _max_bytes: usize,
        ) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async { unreachable!() })
        }

        fn write<'a>(&'a self, _path: &'a str, _content: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { unreachable!() })
        }

        fn execute<'a>(
            &'a self,
            _command: &'a str,
            sandbox_mode: SandboxMode,
            _network_access: NetworkAccess,
            mode: CommandMode,
            output: CommandOutputSink,
        ) -> BoxFuture<'a, Result<CommandOutput>> {
            Box::pin(async move {
                assert_eq!(sandbox_mode, self.expected_sandbox_mode);
                assert_eq!(mode, CommandMode::Background);
                output.write(CommandStream::Stdout, b"first");
                self.started.notify_one();
                self.release.notified().await;
                output.write(CommandStream::Stderr, b"last");
                Ok(CommandOutput {
                    exit_code: 7,
                    stdout: "first".into(),
                    stdout_truncated: false,
                    stderr: "last".into(),
                    stderr_truncated: false,
                })
            })
        }
    }

    #[tokio::test]
    async fn polling_forwards_mode_and_is_incremental_owner_scoped() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let commands = BackgroundCommands::default();
        let id = commands
            .start(
                "session-a",
                Arc::new(StreamingBackend {
                    started: Arc::clone(&started),
                    release: Arc::clone(&release),
                    expected_sandbox_mode: SandboxMode::DangerFullAccess,
                }),
                "command".into(),
                SandboxMode::DangerFullAccess,
                NetworkAccess::Denied,
            )
            .expect("start");
        started.notified().await;

        assert!(commands.has_owner("session-a").expect("owned command"));
        assert!(!commands.has_owner("session-b").expect("foreign command"));
        assert!(commands.poll("session-b", &id).await.is_err());
        let first = commands.poll("session-a", &id).await.expect("first poll");
        assert_eq!(first.status, BackgroundCommandStatus::Running);
        assert_eq!(first.stdout, "first");

        release.notify_one();
        let completed = loop {
            let output = commands.poll("session-a", &id).await.expect("poll");
            if output.status != BackgroundCommandStatus::Running {
                break output;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(completed.status, BackgroundCommandStatus::Exited);
        assert_eq!(completed.exit_code, Some(7));
        assert_eq!(completed.stderr, "last");
        assert!(!commands.has_owner("session-a").expect("completed command"));
        assert!(commands.poll("session-a", &id).await.is_err());
    }

    struct CancellationGuard(Arc<AtomicBool>);

    impl Drop for CancellationGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct PendingBackend {
        started: Arc<Notify>,
        cancelled: Arc<AtomicBool>,
    }

    impl SandboxBackend for PendingBackend {
        fn read<'a>(&'a self, _path: &'a str) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }

        fn read_bytes<'a>(
            &'a self,
            _path: &'a str,
            _max_bytes: usize,
        ) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async { unreachable!() })
        }

        fn write<'a>(&'a self, _path: &'a str, _content: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { unreachable!() })
        }

        fn execute<'a>(
            &'a self,
            _command: &'a str,
            _sandbox_mode: SandboxMode,
            _network_access: NetworkAccess,
            _mode: CommandMode,
            _output: CommandOutputSink,
        ) -> BoxFuture<'a, Result<CommandOutput>> {
            Box::pin(async move {
                let _guard = CancellationGuard(Arc::clone(&self.cancelled));
                self.started.notify_one();
                std::future::pending().await
            })
        }
    }

    #[tokio::test]
    async fn session_shutdown_cancels_its_commands() {
        let started = Arc::new(Notify::new());
        let cancelled = Arc::new(AtomicBool::new(false));
        let commands = BackgroundCommands::default();
        commands
            .start(
                "session-a",
                Arc::new(PendingBackend {
                    started: Arc::clone(&started),
                    cancelled: Arc::clone(&cancelled),
                }),
                "command".into(),
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Denied,
            )
            .expect("start");
        started.notified().await;

        commands.shutdown("session-a").await.expect("shutdown");

        assert!(cancelled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn stopping_a_finished_command_preserves_its_result() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let commands = BackgroundCommands::default();
        let id = commands
            .start(
                "session",
                Arc::new(StreamingBackend {
                    started: Arc::clone(&started),
                    release: Arc::clone(&release),
                    expected_sandbox_mode: SandboxMode::WorkspaceWrite,
                }),
                "command".into(),
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Denied,
            )
            .expect("start");
        started.notified().await;
        release.notify_one();
        while !commands
            .entries
            .lock()
            .expect("commands")
            .get(&id)
            .expect("entry")
            .task
            .is_finished()
        {
            tokio::task::yield_now().await;
        }

        let output = commands.stop("session", &id).await.expect("stop");

        assert_eq!(output.status, BackgroundCommandStatus::Exited);
        assert_eq!(output.exit_code, Some(7));
        assert_eq!(output.stderr, "last");
    }

    #[test]
    fn truncated_poll_buffers_accept_new_output_after_drain() {
        let mut output = BufferedOutput::default();
        output.push(
            CommandStream::Stdout,
            &vec![b'x'; MAX_POLL_OUTPUT_BYTES + 1],
        );
        let first = output.take();
        assert_eq!(first.stdout.len(), MAX_POLL_OUTPUT_BYTES);
        assert!(first.truncated);

        output.push(CommandStream::Stdout, b"next");
        let second = output.take();
        assert_eq!(second.stdout, "next");
        assert!(!second.truncated);
    }
}
