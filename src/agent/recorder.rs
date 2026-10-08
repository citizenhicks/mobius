use std::sync::Arc;

use serde_json::Value;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::{Duration, Instant};

use super::EVENT_QUEUE_CAPACITY;
use super::unix_timestamp_ms;
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::Checkpoint;
use crate::backend::checkpoint::CheckpointStore;
use crate::backend::checkpoint::ExecutionRecord;
use crate::backend::checkpoint::JournalEvent;
use crate::backend::checkpoint::TimestampedEvent;
use crate::protocol::{Event, EventMsg, ModelStepContentPhase};

pub(super) type EventRecorder = Arc<RecorderIngress>;

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
    checkpoint: Arc<Checkpoint>,
    transcript_delta: Vec<Arc<Value>>,
    execution: Option<ExecutionRecord>,
    events: Vec<TimestampedEvent>,
    byte_permit: OwnedSemaphorePermit,
    result: oneshot::Sender<Result<()>>,
}

pub(super) const RECORDER_COMMAND_CAPACITY: usize = 512;
pub(super) const RECORDER_EVENT_BYTE_BUDGET: usize = 8 * 1024 * 1024;
const RECORDER_QUEUE_FULL: &str = "event recorder queue is full";
const STREAM_BATCH_INTERVAL: Duration = Duration::from_millis(40);
const STREAM_BATCH_BYTES: usize = 16 * 1024;

#[derive(PartialEq, Eq)]
struct StreamKey {
    submission_id: Option<Arc<str>>,
    session_id: Arc<str>,
    turn_id: Arc<str>,
    model_step_id: Arc<str>,
    phase: ModelStepContentPhase,
}

impl StreamKey {
    fn of(event: &Event) -> Option<Self> {
        let EventMsg::AssistantContentDelta(delta) = &event.msg else {
            return None;
        };
        Some(Self {
            submission_id: event.submission_id.as_ref().map(Arc::clone),
            session_id: Arc::clone(&delta.session_id),
            turn_id: Arc::clone(&delta.turn_id),
            model_step_id: Arc::clone(&delta.model_step_id),
            phase: delta.phase,
        })
    }
}

struct StreamBatch {
    append: AppendCommand,
    deadline: Instant,
}

impl StreamBatch {
    fn merge(&mut self, command: &mut AppendCommand) -> bool {
        let EventMsg::AssistantContentDelta(pending) = &mut self.append.event.event.msg else {
            return false;
        };
        let EventMsg::AssistantContentDelta(next) = &mut command.event.event.msg else {
            return false;
        };
        if self.append.event.event.submission_id != command.event.event.submission_id
            || pending.session_id != next.session_id
            || pending.turn_id != next.turn_id
            || pending.model_step_id != next.model_step_id
            || pending.phase != next.phase
            || pending.delta.len().saturating_add(next.delta.len()) > STREAM_BATCH_BYTES
        {
            return false;
        }
        pending.delta.push_str(&next.delta);
        true
    }
}

impl RecorderIngress {
    pub(super) fn spawn(
        checkpoints: Arc<dyn CheckpointStore>,
        session_id: String,
    ) -> (Arc<Self>, mpsc::Receiver<JournalEvent>) {
        let (commands, receiver) = mpsc::channel(RECORDER_COMMAND_CAPACITY);
        let (events, event_receiver) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        tokio::spawn(run_recorder(checkpoints, session_id, receiver, events));
        (
            Arc::new(Self {
                commands,
                event_bytes: Arc::new(Semaphore::new(RECORDER_EVENT_BYTE_BUDGET)),
            }),
            event_receiver,
        )
    }

    pub(super) async fn record(&self, event: Event) -> Result<()> {
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

    pub(super) async fn save(
        &self,
        checkpoint: Arc<Checkpoint>,
        transcript_delta: &[Arc<Value>],
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
                checkpoint,
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

    pub(super) async fn flush(&self) -> Result<()> {
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
        Arc::clone(&self.event_bytes)
            .acquire_many_owned(permits)
            .await
            .map_err(|_| Error::Stopped("event recorder stopped".into()))
    }

    fn try_reserve_bytes(&self, bytes: usize) -> Result<OwnedSemaphorePermit> {
        let permits = byte_permits(bytes)?;
        Arc::clone(&self.event_bytes)
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
    let mut pending: Option<StreamBatch> = None;
    let mut last_stream = None;
    loop {
        let command = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(pending.as_ref().map_or_else(Instant::now, |batch| batch.deadline)), if pending.is_some() => {
                terminal_error = flush_stream(&mut pending, &checkpoints, &session_id, &events).await.err();
                continue;
            }
            command = commands.recv() => command,
        };
        let Some(command) = command else {
            let _ = flush_stream(&mut pending, &checkpoints, &session_id, &events).await;
            return;
        };
        if let Some(error) = &terminal_error {
            if reject(command, error) {
                return;
            }
            continue;
        }
        if let RecorderCommand::Append(mut append) = command {
            if let Some(batch) = pending.as_mut()
                && batch.merge(&mut append)
            {
                batch.append.byte_permit.merge(append.byte_permit);
                if let Some(result) = append.result {
                    let _ = result.send(Ok(()));
                }
                continue;
            }
            if let Err(error) = flush_stream(&mut pending, &checkpoints, &session_id, &events).await
            {
                reject(RecorderCommand::Append(append), &error);
                terminal_error = Some(error);
                continue;
            }
            let key = StreamKey::of(&append.event.event);
            if key.is_some()
                && key == last_stream
                && matches!(&append.event.event.msg, EventMsg::AssistantContentDelta(delta) if delta.delta.len() <= STREAM_BATCH_BYTES)
            {
                // Streaming callbacks acknowledge bounded admission; barriers wait for durability.
                if let Some(result) = append.result.take() {
                    let _ = result.send(Ok(()));
                }
                pending = Some(StreamBatch {
                    append: *append,
                    deadline: Instant::now() + STREAM_BATCH_INTERVAL,
                });
            } else {
                last_stream = key;
                let acknowledged = append.result.is_some();
                terminal_error = record_append(*append, &checkpoints, &session_id, &events)
                    .await
                    .err();
                if acknowledged && terminal_error.is_some() {
                    return;
                }
            }
            continue;
        }
        if let Err(error) = flush_stream(&mut pending, &checkpoints, &session_id, &events).await {
            if reject(command, &error) {
                return;
            }
            terminal_error = Some(error);
            continue;
        }
        last_stream = None;
        terminal_error = match command {
            RecorderCommand::Append(_) => unreachable!("appends were handled above"),
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

async fn flush_stream(
    pending: &mut Option<StreamBatch>,
    checkpoints: &Arc<dyn CheckpointStore>,
    session_id: &str,
    events: &mpsc::Sender<JournalEvent>,
) -> std::result::Result<(), RecorderFailure> {
    if let Some(batch) = pending.take() {
        record_append(batch.append, checkpoints, session_id, events).await?;
    }
    Ok(())
}

async fn record_append(
    command: AppendCommand,
    checkpoints: &Arc<dyn CheckpointStore>,
    session_id: &str,
    events: &mpsc::Sender<JournalEvent>,
) -> std::result::Result<(), RecorderFailure> {
    let AppendCommand {
        event,
        byte_permit: _byte_permit,
        result,
    } = command;
    let recorded = match checkpoints
        .append_event(session_id, event.recorded_at_ms, &event.event)
        .await
    {
        Ok(recorded) => recorded,
        Err(error) => {
            let terminal = RecorderFailure::from(&error);
            if let Some(result) = result {
                let _ = result.send(Err(error));
            }
            return Err(terminal);
        }
    };
    let published = publish(recorded, events).await;
    if let Some(result) = result {
        let _ = result.send(Ok(()));
    }
    published.map_err(|error| RecorderFailure::from(&error))
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
