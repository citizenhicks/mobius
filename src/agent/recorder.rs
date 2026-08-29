use std::sync::Arc;

use serde_json::Value;
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
    commands: mpsc::UnboundedSender<RecorderCommand>,
}

enum RecorderCommand {
    Append(Box<AppendCommand>),
    Save(Box<SaveCommand>),
    Flush(oneshot::Sender<Result<()>>),
}

struct AppendCommand {
    event: TimestampedEvent,
    result: Option<oneshot::Sender<Result<()>>>,
}

struct SaveCommand {
    checkpoint: Checkpoint,
    transcript_delta: Vec<Value>,
    execution: Option<ExecutionRecord>,
    events: Vec<TimestampedEvent>,
    result: oneshot::Sender<Result<()>>,
}

impl EventRecorder {
    pub(super) fn spawn(
        checkpoints: Arc<dyn CheckpointStore>,
        session_id: String,
    ) -> (Self, mpsc::Receiver<JournalEvent>) {
        // ponytail: synchronous model sinks cannot await; the model stream byte limit bounds
        // this queue. Batch journal writes if profiling shows recorder memory pressure.
        let (commands, receiver) = mpsc::unbounded_channel();
        let (events, event_receiver) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        tokio::spawn(run_recorder(checkpoints, session_id, receiver, events));
        (Self { commands }, event_receiver)
    }

    pub(super) async fn record(&self, event: Event) -> Result<()> {
        let (result, recorded) = oneshot::channel();
        self.commands
            .send(RecorderCommand::Append(Box::new(AppendCommand {
                event: timestamp(event)?,
                result: Some(result),
            })))
            .map_err(|_| Error::Stopped("event recorder stopped".into()))?;
        recorded
            .await
            .map_err(|_| Error::Stopped("event recorder stopped".into()))?
    }

    pub(super) fn try_record(&self, event: Event) -> Result<()> {
        self.commands
            .send(RecorderCommand::Append(Box::new(AppendCommand {
                event: timestamp(event)?,
                result: None,
            })))
            .map_err(|_| Error::Stopped("event recorder stopped".into()))
    }

    pub(super) async fn save(
        &self,
        checkpoint: &Checkpoint,
        transcript_delta: &[Value],
        execution: Option<&ExecutionRecord>,
        events: Vec<Event>,
    ) -> Result<()> {
        let events = events
            .into_iter()
            .map(timestamp)
            .collect::<Result<Vec<_>>>()?;
        let (result, saved) = oneshot::channel();
        self.commands
            .send(RecorderCommand::Save(Box::new(SaveCommand {
                checkpoint: checkpoint.clone(),
                transcript_delta: transcript_delta.to_vec(),
                execution: execution.cloned(),
                events,
                result,
            })))
            .map_err(|_| Error::Stopped("event recorder stopped".into()))?;
        saved
            .await
            .map_err(|_| Error::Stopped("event recorder stopped".into()))?
    }

    pub(super) async fn flush(&self) -> Result<()> {
        let (flushed, result) = oneshot::channel();
        self.commands
            .send(RecorderCommand::Flush(flushed))
            .map_err(|_| Error::Stopped("event recorder stopped".into()))?;
        result
            .await
            .map_err(|_| Error::Stopped("event recorder stopped".into()))?
    }
}

fn timestamp(event: Event) -> Result<TimestampedEvent> {
    Ok(TimestampedEvent {
        recorded_at_ms: unix_timestamp_ms()?,
        event,
    })
}

async fn run_recorder(
    checkpoints: Arc<dyn CheckpointStore>,
    session_id: String,
    mut commands: mpsc::UnboundedReceiver<RecorderCommand>,
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
                let AppendCommand { event, result } = *command;
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
                    result,
                } = *command;
                let recorded = checkpoints
                    .save_with_events(&checkpoint, &transcript_delta, execution.as_ref(), &pending)
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
