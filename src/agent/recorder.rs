use std::sync::Arc;

use serde_json::Value;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use super::EVENT_QUEUE_CAPACITY;
use super::unix_timestamp_ms;
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::Checkpoint;
use crate::backend::checkpoint::CheckpointStore;
use crate::backend::checkpoint::ExecutionRecord;
use crate::backend::checkpoint::JournalEvent;
use crate::backend::checkpoint::TimestampedEvent;
use crate::protocol::Event;

#[derive(Clone)]
pub(super) struct EventRecorder {
    ingress: Arc<RecorderIngress>,
}

pub(super) struct RecorderIngress {
    commands: mpsc::Sender<RecorderCommand>,
    event_bytes: Arc<Semaphore>,
}

enum RecorderCommand {
    Append(Box<AppendCommand>),
    Save(Box<SaveCommand>),
    Flush(oneshot::Sender<Result<()>>),
}

struct AppendCommand {
    event: TimestampedEvent,
    byte_permit: OwnedSemaphorePermit,
    result: Option<oneshot::Sender<Result<()>>>,
}

struct SaveCommand {
    checkpoint: Checkpoint,
    transcript_delta: Vec<Value>,
    execution: Option<ExecutionRecord>,
    events: Vec<TimestampedEvent>,
    byte_permit: OwnedSemaphorePermit,
    result: oneshot::Sender<Result<()>>,
}

pub(super) const RECORDER_COMMAND_CAPACITY: usize = 512;
pub(super) const RECORDER_EVENT_BYTE_BUDGET: usize = 8 * 1024 * 1024;
const RECORDER_QUEUE_FULL: &str = "event recorder queue is full";

impl EventRecorder {
    pub(super) fn spawn(
        checkpoints: Arc<dyn CheckpointStore>,
        session_id: String,
    ) -> (Self, mpsc::Receiver<JournalEvent>) {
        let (commands, receiver) = mpsc::channel(RECORDER_COMMAND_CAPACITY);
        let (events, event_receiver) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        tokio::spawn(run_recorder(checkpoints, session_id, receiver, events));
        (
            Self {
                ingress: Arc::new(RecorderIngress {
                    commands,
                    event_bytes: Arc::new(Semaphore::new(RECORDER_EVENT_BYTE_BUDGET)),
                }),
            },
            event_receiver,
        )
    }

    pub(super) fn downgrade(&self) -> std::sync::Weak<RecorderIngress> {
        Arc::downgrade(&self.ingress)
    }

    pub(super) async fn record(&self, event: Event) -> Result<()> {
        self.ingress.record(event).await
    }

    pub(super) fn try_record(&self, event: Event) -> Result<()> {
        self.ingress.try_record(event)
    }

    pub(super) async fn save(
        &self,
        checkpoint: &Checkpoint,
        transcript_delta: &[Value],
        execution: Option<&ExecutionRecord>,
        events: Vec<Event>,
    ) -> Result<()> {
        self.ingress
            .save(checkpoint, transcript_delta, execution, events)
            .await
    }

    pub(super) async fn flush(&self) -> Result<()> {
        self.ingress.flush().await
    }
}

impl RecorderIngress {
    async fn record(&self, event: Event) -> Result<()> {
        let (event, event_bytes) = timestamp(event)?;
        let byte_permit = self.reserve_bytes(event_bytes).await?;
        let (result, recorded) = oneshot::channel();
        self.commands
            .send(RecorderCommand::Append(Box::new(AppendCommand {
                event,
                byte_permit,
                result: Some(result),
            })))
            .await
            .map_err(|_| Error::Stopped("event recorder stopped".into()))?;
        recorded
            .await
            .map_err(|_| Error::Stopped("event recorder stopped".into()))?
    }

    pub(super) fn try_record(&self, event: Event) -> Result<()> {
        let (event, event_bytes) = timestamp(event)?;
        let byte_permit = self.try_reserve_bytes(event_bytes)?;
        self.commands
            .try_send(RecorderCommand::Append(Box::new(AppendCommand {
                event,
                byte_permit,
                result: None,
            })))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => queue_full(),
                mpsc::error::TrySendError::Closed(_) => {
                    Error::Stopped("event recorder stopped".into())
                }
            })
    }

    async fn save(
        &self,
        checkpoint: &Checkpoint,
        transcript_delta: &[Value],
        execution: Option<&ExecutionRecord>,
        events: Vec<Event>,
    ) -> Result<()> {
        let mut event_bytes = 0_usize;
        let events = events
            .into_iter()
            .map(|event| {
                let (event, bytes) = timestamp(event)?;
                event_bytes = event_bytes.checked_add(bytes).ok_or_else(queue_full)?;
                Ok(event)
            })
            .collect::<Result<Vec<_>>>()?;
        let byte_permit = self.reserve_bytes(event_bytes).await?;
        let (result, saved) = oneshot::channel();
        self.commands
            .send(RecorderCommand::Save(Box::new(SaveCommand {
                checkpoint: checkpoint.clone(),
                transcript_delta: transcript_delta.to_vec(),
                execution: execution.cloned(),
                events,
                byte_permit,
                result,
            })))
            .await
            .map_err(|_| Error::Stopped("event recorder stopped".into()))?;
        saved
            .await
            .map_err(|_| Error::Stopped("event recorder stopped".into()))?
    }

    async fn flush(&self) -> Result<()> {
        let (flushed, result) = oneshot::channel();
        self.commands
            .send(RecorderCommand::Flush(flushed))
            .await
            .map_err(|_| Error::Stopped("event recorder stopped".into()))?;
        result
            .await
            .map_err(|_| Error::Stopped("event recorder stopped".into()))?
    }

    async fn reserve_bytes(&self, bytes: usize) -> Result<OwnedSemaphorePermit> {
        let permits = byte_permits(bytes)?;
        self.event_bytes
            .clone()
            .acquire_many_owned(permits)
            .await
            .map_err(|_| Error::Stopped("event recorder stopped".into()))
    }

    fn try_reserve_bytes(&self, bytes: usize) -> Result<OwnedSemaphorePermit> {
        let permits = byte_permits(bytes)?;
        self.event_bytes
            .clone()
            .try_acquire_many_owned(permits)
            .map_err(|_| queue_full())
    }
}

fn timestamp(event: Event) -> Result<(TimestampedEvent, usize)> {
    let event_bytes = serde_json::to_vec(&event)?.len();
    Ok((
        TimestampedEvent {
            recorded_at_ms: unix_timestamp_ms()?,
            event,
        },
        event_bytes,
    ))
}

fn byte_permits(bytes: usize) -> Result<u32> {
    if bytes > RECORDER_EVENT_BYTE_BUDGET {
        return Err(queue_full());
    }
    u32::try_from(bytes).map_err(|_| queue_full())
}

fn queue_full() -> Error {
    Error::Stopped(RECORDER_QUEUE_FULL.into())
}

async fn run_recorder(
    checkpoints: Arc<dyn CheckpointStore>,
    session_id: String,
    mut commands: mpsc::Receiver<RecorderCommand>,
    events: mpsc::Sender<JournalEvent>,
) {
    let mut terminal_error = None;
    while let Some(command) = commands.recv().await {
        if let Some(error) = &terminal_error {
            if reject(command, error) {
                return;
            }
            continue;
        }
        terminal_error = match command {
            RecorderCommand::Append(command) => {
                let AppendCommand {
                    event,
                    byte_permit: _byte_permit,
                    result,
                } = *command;
                let recorded = checkpoints
                    .append_event(&session_id, event.recorded_at_ms, &event.event)
                    .await;
                let recorded = match recorded {
                    Ok(recorded) => recorded,
                    Err(error) => {
                        let terminal = RecorderFailure::from(&error);
                        if let Some(result) = result {
                            let _ = result.send(Err(error));
                            return;
                        }
                        terminal_error = Some(terminal);
                        continue;
                    }
                };
                match publish(recorded, &events).await {
                    Ok(()) => {
                        if let Some(result) = result {
                            let _ = result.send(Ok(()));
                        }
                        None
                    }
                    Err(error) => {
                        let terminal = RecorderFailure::from(&error);
                        if let Some(result) = result {
                            let _ = result.send(Ok(()));
                            return;
                        }
                        Some(terminal)
                    }
                }
            }
            RecorderCommand::Save(command) => {
                let SaveCommand {
                    checkpoint,
                    transcript_delta,
                    execution,
                    events: pending,
                    byte_permit: _byte_permit,
                    result,
                } = *command;
                let recorded = checkpoints
                    .save_with_events(checkpoint, transcript_delta, execution, pending)
                    .await;
                let recorded = match recorded {
                    Ok(recorded) => recorded,
                    Err(error) => {
                        let _ = result.send(Err(error));
                        return;
                    }
                };
                match publish_all(recorded, &events).await {
                    Ok(()) => {
                        let _ = result.send(Ok(()));
                        None
                    }
                    Err(_) => {
                        let _ = result.send(Ok(()));
                        return;
                    }
                }
            }
            RecorderCommand::Flush(result) => {
                let _ = result.send(Ok(()));
                None
            }
        };
    }
}

struct RecorderFailure {
    message: String,
}

impl From<&Error> for RecorderFailure {
    fn from(error: &Error) -> Self {
        let message = match error {
            Error::Stopped(message) => message.clone(),
            error => error.to_string(),
        };
        Self { message }
    }
}

impl RecorderFailure {
    fn error(&self) -> Error {
        Error::Stopped(self.message.clone())
    }
}

fn reject(command: RecorderCommand, failure: &RecorderFailure) -> bool {
    match command {
        RecorderCommand::Append(command) => {
            if let Some(result) = command.result {
                let _ = result.send(Err(failure.error()));
            }
            false
        }
        RecorderCommand::Save(command) => {
            let _ = command.result.send(Err(failure.error()));
            false
        }
        RecorderCommand::Flush(result) => {
            let _ = result.send(Err(failure.error()));
            true
        }
    }
}

async fn publish(record: JournalEvent, events: &mpsc::Sender<JournalEvent>) -> Result<()> {
    events
        .send(record)
        .await
        .map_err(|_| Error::Stopped("frontend event channel closed".into()))
}

async fn publish_all(
    records: Vec<JournalEvent>,
    events: &mpsc::Sender<JournalEvent>,
) -> Result<()> {
    for record in records {
        events
            .send(record)
            .await
            .map_err(|_| Error::Stopped("frontend event channel closed".into()))?;
    }
    Ok(())
}
