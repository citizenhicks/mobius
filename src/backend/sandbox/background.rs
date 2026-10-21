use crate::SessionId;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::{
    CommandMode, CommandOutput, CommandOutputSink, CommandStream, NetworkAccess, SandboxBackend,
    SandboxMode,
};
use crate::{Error, Result};

const MAX_POLL_OUTPUT_BYTES: usize = 6_000;
const MAX_ERROR_BYTES: usize = 512;
const MAX_NOTICE_COMMAND_BYTES: usize = 200;
const HELD: u8 = 1;
const FINISHED: u8 = 2;

/// Receives the command ID and notice of a finished command whose ID the model holds.
pub(super) type BackgroundListener = Arc<dyn Fn(&str, String) + Send + Sync>;

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
    pub(crate) command_id: Option<String>,
    pub(crate) status: BackgroundCommandStatus,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) truncated: bool,
    pub(crate) error: Option<String>,
}

pub(super) struct BackgroundCommands {
    limit: usize,
    listeners: Mutex<BTreeMap<SessionId, BackgroundListener>>,
    entries: Mutex<BTreeMap<Arc<str>, Entry>>,
}

impl Default for BackgroundCommands {
    fn default() -> Self {
        Self::new(super::default_background_command_limit())
    }
}

struct Entry {
    owner: SessionId,
    started: std::time::Instant,
    // Shared with the command task, which notifies only once the model holds the ID.
    state: Arc<AtomicU8>,
    output: Arc<Mutex<BufferedOutput>>,
    task: JoinHandle<Result<CommandOutput>>,
}

// Until its ID reaches the caller, cancellation must not leave a command running.
pub(super) struct StartingCommand<'a> {
    commands: &'a BackgroundCommands,
    id: Arc<str>,
    completion: oneshot::Receiver<()>,
    delivered: bool,
}

impl StartingCommand<'_> {
    pub(super) async fn wait(
        mut self,
        owner: &SessionId,
        initial_wait: std::time::Duration,
    ) -> Result<BackgroundCommandPoll> {
        // Completion and an elapsed wait both continue to the poll, which reports the state.
        if let Err(elapsed) = tokio::time::timeout(initial_wait, &mut self.completion).await {
            tracing::trace!(%elapsed, "background command still running after its initial wait");
        }
        let output = self.commands.poll(owner, &self.id).await?;
        self.delivered = true;
        Ok(output)
    }
}

impl Drop for StartingCommand<'_> {
    fn drop(&mut self) {
        if !self.delivered
            && let Ok(mut entries) = self.commands.entries.lock()
            && let Some(entry) = entries.remove(&self.id)
        {
            entry.task.abort();
        }
    }
}

#[derive(Default)]
struct BufferedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    saw_output: bool,
}

impl BackgroundCommands {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            listeners: Mutex::new(BTreeMap::new()),
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    pub(super) fn listen(&self, owner: &SessionId, listener: BackgroundListener) -> Result<()> {
        self.listeners
            .lock()
            .map_err(state_error)?
            .insert(owner.share(), listener);
        Ok(())
    }

    pub(super) fn limit(&self) -> usize {
        self.limit
    }

    pub(super) fn has_owner(&self, owner: &SessionId) -> Result<bool> {
        Ok(self
            .entries
            .lock()
            .map_err(state_error)?
            .values()
            .any(|entry| &entry.owner == owner && !entry.task.is_finished()))
    }

    pub(super) fn start(
        &self,
        owner: &SessionId,
        backend: Arc<dyn SandboxBackend>,
        command: String,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
    ) -> Result<StartingCommand<'_>> {
        let listener = self
            .listeners
            .lock()
            .map_err(state_error)?
            .get(owner)
            .map(Arc::clone);
        let mut entries = self.entries.lock().map_err(state_error)?;
        if entries.len() >= self.limit {
            // Never evict a result before its initial wait has returned it to the caller.
            let oldest = entries
                .values()
                .filter(|entry| {
                    entry.state.load(Ordering::Relaxed) & HELD != 0 && entry.task.is_finished()
                })
                .map(|entry| entry.started)
                .min();
            let Some(oldest) = oldest else {
                let own = entries
                    .iter()
                    .filter(|(_, entry)| &entry.owner == owner)
                    .map(|(id, _)| &**id)
                    .collect::<Vec<_>>();
                return Err(Error::Sandbox(format!(
                    "background command limit {} reached; stop a command you no longer need, or wait for one to finish, then retry. {}",
                    self.limit,
                    if own.is_empty() {
                        "None of the running commands belong to this chat.".into()
                    } else {
                        format!("Your commands: {}", own.join(", "))
                    }
                )));
            };
            entries
                .extract_if(.., |_, entry| {
                    entry.state.load(Ordering::Relaxed) & HELD != 0
                        && entry.started == oldest
                        && entry.task.is_finished()
                })
                .next();
        }
        let id = loop {
            let candidate = Uuid::new_v4().to_string();
            if !entries.contains_key(candidate.as_str()) {
                break Arc::<str>::from(candidate);
            }
        };
        let output = Arc::new(Mutex::new(BufferedOutput::default()));
        let streamed = Arc::clone(&output);
        let sink = CommandOutputSink::new(move |stream, bytes| {
            if let Ok(mut output) = streamed.lock() {
                output.push(stream, bytes);
            }
        });
        let (finished, completion) = oneshot::channel();
        let state = Arc::new(AtomicU8::new(0));
        let task_state = Arc::clone(&state);
        let task_id = Arc::clone(&id);
        let task = tokio::spawn(async move {
            let result = backend
                .execute(
                    &command,
                    sandbox_mode,
                    network_access,
                    CommandMode::Background,
                    sink,
                )
                .await;
            // One atomic handoff chooses either a final poll result or a later completion notice.
            let held = task_state.fetch_or(FINISHED, Ordering::Relaxed) & HELD != 0;
            if let Some(listener) = listener
                && held
            {
                listener(&task_id, completion_notice(&task_id, &command, &result));
            }
            if finished.send(()).is_err() {
                tracing::trace!("background command finished after its initial wait ended");
            }
            result
        });
        entries.insert(
            Arc::clone(&id),
            Entry {
                owner: owner.share(),
                started: std::time::Instant::now(),
                state,
                output,
                task,
            },
        );
        drop(entries);
        Ok(StartingCommand {
            commands: self,
            id,
            completion,
            delivered: false,
        })
    }

    pub(super) async fn poll(&self, owner: &SessionId, id: &str) -> Result<BackgroundCommandPoll> {
        let entry = {
            let mut entries = self.entries.lock().map_err(state_error)?;
            let entry = entries.get_mut(id).ok_or_else(|| unknown(id))?;
            ensure_owner(entry, owner, id)?;
            if !entry.task.is_finished()
                && entry.state.fetch_or(HELD, Ordering::Relaxed) & FINISHED == 0
            {
                return poll_running(id, &entry.output);
            }
            entries.remove(id).ok_or_else(|| unknown(id))?
        };
        completed(entry).await
    }

    pub(super) async fn stop(&self, owner: &SessionId, id: &str) -> Result<BackgroundCommandPoll> {
        let entry = {
            let mut entries = self.entries.lock().map_err(state_error)?;
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

    pub(super) async fn shutdown(&self, owner: &SessionId) -> Result<()> {
        self.listeners.lock().map_err(state_error)?.remove(owner);
        let entries = {
            let mut entries = self.entries.lock().map_err(state_error)?;
            entries
                .extract_if(.., |_, entry| &entry.owner == owner)
                .map(|(_, entry)| entry)
                .collect::<Vec<_>>()
        };
        for entry in &entries {
            entry.task.abort();
        }
        for entry in entries {
            match entry.task.await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::debug!(%error, "background command failed before shutdown");
                }
                Err(error) if error.is_cancelled() => {}
                Err(error) => tracing::warn!(%error, "background command task failed on shutdown"),
            }
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
        let (kept, dropped) = bytes.split_at(bytes.len().min(remaining));
        match stream {
            CommandStream::Stdout => self.stdout.extend_from_slice(kept),
            CommandStream::Stderr => self.stderr.extend_from_slice(kept),
        }
        self.truncated |= !dropped.is_empty();
    }

    fn take(&mut self) -> TakenOutput {
        TakenOutput {
            stdout: String::from_utf8_lossy_owned(std::mem::take(&mut self.stdout)),
            stderr: String::from_utf8_lossy_owned(std::mem::take(&mut self.stderr)),
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

fn poll_running(id: &str, output: &Arc<Mutex<BufferedOutput>>) -> Result<BackgroundCommandPoll> {
    let output = take_output(output)?;
    Ok(BackgroundCommandPoll {
        command_id: Some(id.into()),
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
        command_id: None,
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
                command_id: None,
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
                command_id: None,
                status: BackgroundCommandStatus::Failed,
                exit_code: None,
                stdout: output.stdout,
                stderr: output.stderr,
                truncated: output.truncated || truncated,
                error: Some(crate::truncate_utf8(&error, MAX_ERROR_BYTES).into()),
            })
        }
        Err(error) => Ok(BackgroundCommandPoll {
            command_id: None,
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

fn completion_notice(id: &str, command: &str, result: &Result<CommandOutput>) -> String {
    let text = &super::text::DEFINITION;
    let outcome = match result {
        Ok(output) => text
            .background_command_exited
            .replace("{exit_code}", &output.exit_code.to_string()),
        Err(error) => text.background_command_failed.replace(
            "{error}",
            crate::truncate_utf8(&error.to_string(), MAX_ERROR_BYTES),
        ),
    };
    text.background_command_notice
        .replace("{id}", id)
        .replace(
            "{command}",
            crate::truncate_utf8(command, MAX_NOTICE_COMMAND_BYTES),
        )
        .replace("{outcome}", &outcome)
}

fn take_output(output: &Arc<Mutex<BufferedOutput>>) -> Result<TakenOutput> {
    Ok(output.lock().map_err(state_error)?.take())
}

fn ensure_owner(entry: &Entry, owner: &SessionId, id: &str) -> Result<()> {
    if &entry.owner != owner {
        return Err(unknown(id));
    }
    Ok(())
}

fn unknown(id: &str) -> Error {
    Error::Unknown(format!("background command `{id}`"))
}

fn state_error(error: impl std::fmt::Display) -> Error {
    Error::Stopped(format!("background command state lock poisoned: {error}"))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::sync::Notify;

    use super::*;
    use crate::BoxFuture;

    struct CancellationGuard(Arc<AtomicBool>);

    impl Drop for CancellationGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct PendingBackend {
        started: Arc<Notify>,
        cancelled: Arc<AtomicBool>,
        finish: Option<Arc<Notify>>,
    }

    impl SandboxBackend for PendingBackend {
        fn read<'a>(
            &'a self,
            _path: &'a str,
            _sandbox_mode: SandboxMode,
        ) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }

        fn read_bytes<'a>(
            &'a self,
            _path: &'a str,
            _max_bytes: usize,
            _sandbox_mode: SandboxMode,
        ) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async { unreachable!() })
        }

        fn write<'a>(
            &'a self,
            _path: &'a str,
            _content: &'a str,
            _sandbox_mode: SandboxMode,
        ) -> BoxFuture<'a, Result<()>> {
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
                match &self.finish {
                    Some(finish) => finish.notified().await,
                    None => std::future::pending().await,
                }
                Ok(command_output())
            })
        }
    }

    fn command_output() -> CommandOutput {
        CommandOutput {
            exit_code: 3,
            stdout: "finished".into(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[tokio::test]
    async fn completed_command_is_not_evicted_before_initial_delivery() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend: Arc<dyn SandboxBackend> = Arc::new(
            crate::backend::sandbox::local::LocalSandbox::new(workspace.path()).expect("sandbox"),
        );
        let commands = BackgroundCommands::new(1);
        let session: SessionId = "session".parse().expect("session ID");
        let (notices, mut received) = tokio::sync::mpsc::unbounded_channel();
        commands
            .listen(
                &session,
                Arc::new(move |_, notice| {
                    notices.send(notice).expect("notice receiver");
                }),
            )
            .expect("listener");
        let start = || {
            commands.start(
                &session,
                Arc::clone(&backend),
                "printf first".into(),
                SandboxMode::DangerFullAccess,
                NetworkAccess::Denied,
            )
        };
        let first = start().expect("first command");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !commands.entries.lock().expect("entries")[&first.id]
                .task
                .is_finished()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first command finished");

        assert!(start().is_err());
        let output = first
            .wait(&session, std::time::Duration::ZERO)
            .await
            .expect("first result");
        assert_eq!(output.status, BackgroundCommandStatus::Exited);
        assert_eq!(output.stdout, "first");
        assert!(
            received.try_recv().is_err(),
            "collected command sent a notice"
        );
        start().expect("collected result releases capacity");
    }

    #[tokio::test]
    async fn session_listeners_receive_only_their_held_commands() {
        let commands = BackgroundCommands::default();
        let (notices, mut received) = tokio::sync::mpsc::unbounded_channel();
        for name in ["session-a", "session-b"] {
            let session: SessionId = name.parse().expect("session ID");
            let notices = notices.clone();
            commands
                .listen(
                    &session,
                    Arc::new(move |id, notice| {
                        notices
                            .send((name, id.to_owned(), notice))
                            .expect("notice receiver");
                    }),
                )
                .expect("listener");
        }
        let mut pending = Vec::new();
        for name in ["session-a", "session-b"] {
            let session: SessionId = name.parse().expect("session ID");
            let finish = Arc::new(Notify::new());
            let started = Arc::new(Notify::new());
            let command = commands
                .start(
                    &session,
                    Arc::new(PendingBackend {
                        started: Arc::clone(&started),
                        cancelled: Arc::default(),
                        finish: Some(Arc::clone(&finish)),
                    }),
                    name.into(),
                    SandboxMode::DangerFullAccess,
                    NetworkAccess::Denied,
                )
                .expect("start");
            started.notified().await;
            let held = command
                .wait(&session, std::time::Duration::ZERO)
                .await
                .expect("held");
            assert_eq!(held.status, BackgroundCommandStatus::Running);
            pending.push((session, held.command_id.expect("command ID"), finish));
        }
        for (session, id, finish) in pending {
            finish.notify_one();
            let (owner, noticed_id, notice) =
                tokio::time::timeout(std::time::Duration::from_secs(5), received.recv())
                    .await
                    .expect("completion notice")
                    .expect("notice sender");
            assert_eq!(owner, session.as_str());
            assert_eq!(noticed_id, id);
            assert!(notice.contains("exited with code 3"));
            commands.shutdown(&session).await.expect("shutdown");
            assert!(
                !commands
                    .listeners
                    .lock()
                    .expect("listeners")
                    .contains_key(&session)
            );
        }
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn poll_collects_a_finished_result_before_the_task_returns() {
        let commands = BackgroundCommands::default();
        let session: SessionId = "session".parse().expect("session ID");
        let finish = Arc::new(Notify::new());
        let task_finish = Arc::clone(&finish);
        commands.entries.lock().expect("entries").insert(
            "command".into(),
            Entry {
                owner: session.share(),
                started: std::time::Instant::now(),
                state: Arc::new(AtomicU8::new(FINISHED)),
                output: Arc::default(),
                task: tokio::spawn(async move {
                    task_finish.notified().await;
                    Ok(command_output())
                }),
            },
        );
        let mut polling = Box::pin(commands.poll(&session, "command"));
        tokio::select! {
            biased;
            _ = &mut polling => panic!("must await the finished result"),
            () = tokio::task::yield_now() => {}
        }
        finish.notify_one();
        let output = polling.await.expect("finished result");
        assert_eq!(output.status, BackgroundCommandStatus::Exited);
        assert_eq!(output.stdout, "finished");
    }

    #[tokio::test]
    async fn session_shutdown_cancels_its_commands() {
        let started = Arc::new(Notify::new());
        let cancelled = Arc::new(AtomicBool::new(false));
        let commands = BackgroundCommands::default();
        let _command = commands
            .start(
                &"session-a".parse().expect("session ID"),
                Arc::new(PendingBackend {
                    started: Arc::clone(&started),
                    cancelled: Arc::clone(&cancelled),
                    finish: None,
                }),
                "command".into(),
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Denied,
            )
            .expect("start");
        started.notified().await;

        commands
            .shutdown(&"session-a".parse().expect("session ID"))
            .await
            .expect("shutdown");

        assert!(cancelled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancelling_the_initial_wait_stops_the_undelivered_command() {
        let started = Arc::new(Notify::new());
        let cancelled = Arc::new(AtomicBool::new(false));
        let commands = BackgroundCommands::default();
        let session: SessionId = "session".parse().expect("session ID");
        let command = commands
            .start(
                &session,
                Arc::new(PendingBackend {
                    started: Arc::clone(&started),
                    cancelled: Arc::clone(&cancelled),
                    finish: None,
                }),
                "command".into(),
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Denied,
            )
            .expect("start");
        let mut waiting = Box::pin(command.wait(&session, std::time::Duration::from_secs(60)));
        tokio::select! {
            _ = &mut waiting => panic!("command must still be running"),
            () = started.notified() => {}
        }
        drop(waiting);
        assert!(!commands.has_owner(&session).expect("no orphan command"));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !cancelled.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("command cancelled");
    }
}
