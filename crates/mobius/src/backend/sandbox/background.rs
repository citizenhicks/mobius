use crate::SessionId;
use std::collections::BTreeMap;
use std::mem::DropGuard;
use std::time::Duration;

use crate::middleware::FrontendEventSink;
use crate::protocol::{
    FrontendActionListItem, FrontendEvent, FrontendListItemState, FrontendSlot, FrontendSymbol,
    FrontendWidget, FrontendWidgetContent,
};
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

/// Session footer widget reporting the sandbox's active commands.
pub const RUNNING_COMMANDS_WIDGET: &str = "running-commands";

const MAX_POLL_OUTPUT_BYTES: usize = 6_000;
const MAX_ERROR_BYTES: usize = 512;
const MAX_NOTICE_COMMAND_BYTES: usize = 200;
const HELD: u8 = 1;
const FINISHED: u8 = 2;
const FAILED: u8 = 4;
const MAX_RECENT_RESULTS: usize = 8;
const USER_STOP: u8 = 8;
pub(super) const STOP_COMMAND: &str = "stop_command";
const SHOW_AFTER: Duration = Duration::from_millis(250);

/// Receives the command ID and notice of a finished command whose ID the model holds.
pub(super) type BackgroundListener = Arc<dyn Fn(&str, String) + Send + Sync>;

struct SessionListener {
    notice: BackgroundListener,
    frontend: FrontendEventSink,
    rows: Mutex<Vec<FrontendActionListItem>>,
}

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
    listeners: Mutex<BTreeMap<SessionId, Arc<SessionListener>>>,
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
    preview: String,
    state: Arc<AtomicU8>,
    listener: Option<Arc<SessionListener>>,
    delivered: bool,
}

impl StartingCommand<'_> {
    pub(super) async fn wait(
        mut self,
        owner: &SessionId,
        initial_wait: std::time::Duration,
    ) -> Result<BackgroundCommandPoll> {
        // Fast foreground commands never flash a pill or broadcast the chat catalog.
        let delay = initial_wait.min(SHOW_AFTER);
        let deadline = tokio::time::Instant::now() + initial_wait;
        if tokio::time::timeout(delay, &mut self.completion)
            .await
            .is_err()
        {
            if let Some(listener) = &self.listener
                && let Err(error) =
                    listener.started(&self.id, std::mem::take(&mut self.preview), &self.state)
            {
                tracing::warn!(%error, "running command could not be displayed");
            }
            if initial_wait > delay {
                let _elapsed = tokio::time::timeout_at(deadline, &mut self.completion).await;
            }
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
            entries: Mutex::default(),
        }
    }

    pub(super) fn listen(
        &self,
        owner: &SessionId,
        notice: BackgroundListener,
        frontend: FrontendEventSink,
    ) -> Result<()> {
        self.listeners.lock().map_err(state_error)?.insert(
            owner.share(),
            Arc::new(SessionListener {
                notice,
                frontend,
                rows: Mutex::default(),
            }),
        );
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
        observer: CommandOutputSink,
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
            observer.write(stream, bytes);
        });
        let (finished, completion) = oneshot::channel();
        let state = Arc::new(AtomicU8::new(0));
        let task_state = Arc::clone(&state);
        let task_id = Arc::clone(&id);
        // The transcript retains the complete script. Cap the text carried in live lists.
        let prefix = crate::truncate_utf8(&command, 1024);
        let preview = if prefix.len() == command.len() {
            prefix.into()
        } else {
            format!("{prefix}…")
        };
        let activity = listener.as_ref().map(|listener| {
            let listener = Arc::clone(listener);
            let id = Arc::clone(&id);
            // Aborts and panics must dismiss the pill just like a normal exit.
            DropGuard::new(Arc::clone(&state), move |state| {
                let previous = state.fetch_or(FINISHED, Ordering::Relaxed);
                if previous & (USER_STOP | HELD | FINISHED) == (USER_STOP | HELD) {
                    let text = super::text::DEFINITION
                        .background_command_stopped
                        .replace("{id}", &id);
                    (listener.notice)(&id, text);
                }
                let outcome = if previous & FAILED != 0 || std::thread::panicking() {
                    FrontendListItemState::Failed
                } else if previous & FINISHED != 0 {
                    FrontendListItemState::Completed
                } else {
                    FrontendListItemState::Interrupted
                };
                if let Err(error) = listener.finished(&id, outcome) {
                    tracing::warn!(%error, "command result could not be displayed");
                }
            })
        });
        let notice = listener
            .as_ref()
            .map(|listener| Arc::clone(&listener.notice));
        let task = tokio::spawn(async move {
            let _activity = activity;
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
            let outcome = if result.as_ref().is_ok_and(|output| output.exit_code == 0) {
                FINISHED
            } else {
                FINISHED | FAILED
            };
            let held = task_state.fetch_or(outcome, Ordering::Relaxed) & HELD != 0;
            if let Some(listener) = notice
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
                state: Arc::clone(&state),
                output,
                task,
            },
        );
        drop(entries);
        Ok(StartingCommand {
            commands: self,
            id,
            completion,
            preview,
            state,
            listener,
            delivered: false,
        })
    }

    pub(super) async fn poll(&self, owner: &SessionId, id: &str) -> Result<BackgroundCommandPoll> {
        let entry = {
            let mut entries = self.entries.lock().map_err(state_error)?;
            let entry = owned_entry(&entries, owner, id)?;
            if !entry.task.is_finished()
                && entry.state.load(Ordering::Relaxed) & USER_STOP == 0
                && entry.state.fetch_or(HELD, Ordering::Relaxed) & FINISHED == 0
            {
                return poll_running(id, &entry.output);
            }
            entries.remove(id).ok_or_else(|| unknown(id))?
        };
        completed(entry).await
    }

    // The waiting tool still owns its result, including output produced before a user stop.
    pub(super) fn request_user_stop(&self, owner: &SessionId, id: &str) -> Result<()> {
        let entries = self.entries.lock().map_err(state_error)?;
        let entry = owned_entry(&entries, owner, id)?;
        entry.state.fetch_or(USER_STOP, Ordering::Relaxed);
        entry.task.abort();
        drop(entries);
        Ok(())
    }

    pub(super) async fn stop(&self, owner: &SessionId, id: &str) -> Result<BackgroundCommandPoll> {
        let entry = {
            let mut entries = self.entries.lock().map_err(state_error)?;
            owned_entry(&entries, owner, id)?;
            entries.remove(id).ok_or_else(|| unknown(id))?
        };
        entry.task.abort();
        completed(entry).await
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
        if let Ok(entries) = self.entries.lock() {
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

impl TakenOutput {
    fn into_poll(self, status: BackgroundCommandStatus) -> BackgroundCommandPoll {
        BackgroundCommandPoll {
            command_id: None,
            status,
            exit_code: None,
            stdout: self.stdout,
            stderr: self.stderr,
            truncated: self.truncated,
            error: None,
        }
    }
}

fn poll_running(id: &str, output: &Arc<Mutex<BufferedOutput>>) -> Result<BackgroundCommandPoll> {
    Ok(BackgroundCommandPoll {
        command_id: Some(id.into()),
        ..take_output(output)?.into_poll(BackgroundCommandStatus::Running)
    })
}

async fn completed(entry: Entry) -> Result<BackgroundCommandPoll> {
    let result = entry.task.await;
    finish(&entry.output, result)
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
                exit_code: Some(command.exit_code),
                ..output.into_poll(BackgroundCommandStatus::Exited)
            })
        }
        Ok(Err(error)) => {
            let error = error.to_string();
            output.truncated |= error.len() > MAX_ERROR_BYTES;
            Ok(BackgroundCommandPoll {
                error: Some(crate::truncate_utf8(&error, MAX_ERROR_BYTES).into()),
                ..output.into_poll(BackgroundCommandStatus::Failed)
            })
        }
        Err(error) if error.is_cancelled() => {
            Ok(output.into_poll(BackgroundCommandStatus::Stopped))
        }
        Err(_) => Ok(BackgroundCommandPoll {
            error: Some("background command task failed".into()),
            ..output.into_poll(BackgroundCommandStatus::Failed)
        }),
    }
}

impl SessionListener {
    fn started(&self, id: &Arc<str>, text: String, state: &AtomicU8) -> Result<()> {
        let mut rows = self.rows.lock().map_err(state_error)?;
        if state.load(Ordering::Relaxed) & FINISHED != 0 {
            return Ok(());
        }
        rows.insert(
            0,
            FrontendActionListItem {
                id: Arc::clone(id),
                text,
                outcome: None,
                state: FrontendListItemState::InProgress,
                actions: vec![crate::protocol::FrontendAction {
                    id: format!("stop-{id}"),
                    label: super::text::DEFINITION.stop_command_label.clone(), // rules-allow: each published action owns the manifest label
                    symbol: FrontendSymbol::Custom("stop".into()),
                    tone: crate::protocol::FrontendTone::Warning,
                    op: crate::protocol::Op::command(
                        super::MANIFEST.id,
                        STOP_COMMAND,
                        id.to_string(),
                    ),
                    input_from_label: false,
                    editor: None,
                }],
            },
        );
        self.publish(rows)
    }

    fn finished(&self, id: &str, outcome: FrontendListItemState) -> Result<()> {
        let mut rows = self.rows.lock().map_err(state_error)?;
        let Some(index) = rows.iter().position(|row| row.id.as_ref() == id) else {
            return Ok(()); // A fast foreground command was never displayed.
        };
        let mut row = rows.remove(index);
        row.state = outcome;
        row.actions.clear();
        let active = rows.partition_point(|row| row.state == FrontendListItemState::InProgress);
        rows.insert(active, row);
        rows.truncate(active + MAX_RECENT_RESULTS);
        self.publish(rows)
    }

    // The row lock orders snapshots, even when several commands exit together.
    fn publish(&self, rows: std::sync::MutexGuard<'_, Vec<FrontendActionListItem>>) -> Result<()> {
        let count = rows.partition_point(|row| row.state == FrontendListItemState::InProgress);
        let title = &super::text::DEFINITION.running_commands_title;
        let widget = FrontendWidget {
            count: Some(count),
            icon_only: true,
            symbol: Some(FrontendSymbol::Custom("terminal".into())),
            content: Some(FrontendWidgetContent::ActionList {
                title: title.to_owned(),
                items: rows.to_vec(),
                actions: Vec::new(),
            }),
            ..FrontendWidget::new(
                RUNNING_COMMANDS_WIDGET,
                FrontendSlot::ComposerFooter,
                format!("{title} · {count}"),
            )
        };
        let result = (self.frontend)(FrontendEvent::Widget {
            capability: super::MANIFEST.id.into(),
            item: Arc::new(widget),
        });
        drop(rows);
        result
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

fn owned_entry<'a>(
    entries: &'a BTreeMap<Arc<str>, Entry>,
    owner: &SessionId,
    id: &str,
) -> Result<&'a Entry> {
    entries
        .get(id)
        .filter(|entry| &entry.owner == owner)
        .ok_or_else(|| unknown(id))
}

fn unknown(id: &str) -> Error {
    Error::Unknown(format!("background command `{id}`"))
}

fn state_error(error: impl std::fmt::Display) -> Error {
    Error::Stopped(format!("background command state lock poisoned: {error}"))
}

#[cfg(test)]
mod tests;
