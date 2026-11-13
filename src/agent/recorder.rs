use crate::SessionId;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::{Duration, Instant};

use super::EVENT_QUEUE_CAPACITY;
use super::unix_timestamp_ms;
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::Checkpoint;
use crate::backend::checkpoint::CheckpointStore;
use crate::backend::checkpoint::CommittedRunStats;
use crate::backend::checkpoint::ExecutionRecord;
use crate::backend::checkpoint::JournalEvent;
use crate::backend::checkpoint::RunStats;
use crate::backend::checkpoint::TimestampedEvent;
use crate::backend::checkpoint::ValidatedCheckpoint;
use crate::protocol::{Event, EventMsg, ModelStepContentPhase};

pub(super) type EventRecorder = Arc<RecorderIngress>;

pub(super) struct RecorderIngress {
    commands: mpsc::Sender<RecorderCommand>,
    event_bytes: Arc<Semaphore>,
    stats: watch::Receiver<Option<CommittedRunStats>>,
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
    checkpoint: ValidatedCheckpoint,
    stats: Option<Arc<RunStats>>,
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
    session_id: SessionId,
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
            session_id: delta.session_id.share(),
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
        session_id: SessionId,
    ) -> (Arc<Self>, mpsc::Receiver<JournalEvent>) {
        let (commands, receiver) = mpsc::channel(RECORDER_COMMAND_CAPACITY);
        let (events, event_receiver) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        let (stats, latest_stats) = watch::channel(None);
        tokio::spawn(run_recorder(
            checkpoints,
            session_id,
            receiver,
            events,
            stats,
        ));
        (
            Arc::new(Self {
                commands,
                stats: latest_stats,
                event_bytes: Arc::new(Semaphore::new(RECORDER_EVENT_BYTE_BUDGET)),
            }),
            event_receiver,
        )
    }

    pub(super) fn subscribe_run_stats(&self) -> watch::Receiver<Option<CommittedRunStats>> {
        let mut stats = self.stats.clone(); // rules-allow: every observer owns a watch cursor, not a statistics payload copy
        stats.mark_unchanged();
        stats
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
            .map_err(stopped)?;
        recorded.await.map_err(stopped)?
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
        execution: Option<ExecutionRecord>,
        events: Vec<Event>,
    ) -> Result<()> {
        let checkpoint = ValidatedCheckpoint::new(checkpoint)?;
        let mut event_bytes = 0_usize;
        let events = events
            .into_iter()
            .map(|event| {
                let (event, bytes) = timestamp(event)?;
                event_bytes = event_bytes.checked_add(bytes).ok_or_else(queue_full)?;
                Ok(event)
            })
            .collect::<Result<Vec<_>>>()?;
        let stats = events
            .iter()
            .rev()
            .find(|event| {
                matches!(
                    event.event.msg,
                    EventMsg::TurnStarted(_)
                        | EventMsg::TurnComplete(_)
                        | EventMsg::TurnAborted(_)
                        | EventMsg::ModelStepStarted(_)
                        | EventMsg::ModelStepCompleted(_)
                        | EventMsg::ToolCallEnd(_)
                        | EventMsg::TokenCount(_)
                )
            })
            .map(|event| Arc::new(RunStats::from_checkpoint(&checkpoint, event.recorded_at_ms)));
        let byte_permit = self.reserve_bytes(event_bytes).await?;
        let (result, saved) = oneshot::channel();
        self.commands
            .send(RecorderCommand::Save(Box::new(SaveCommand {
                checkpoint,
                stats,
                transcript_delta: transcript_delta.to_vec(),
                execution,
                events,
                byte_permit,
                result,
            })))
            .await
            .map_err(stopped)?;
        saved.await.map_err(stopped)?
    }

    pub(super) async fn flush(&self) -> Result<()> {
        let (flushed, result) = oneshot::channel();
        self.commands
            .send(RecorderCommand::Flush(flushed))
            .await
            .map_err(stopped)?;
        result.await.map_err(stopped)?
    }

    async fn reserve_bytes(&self, bytes: usize) -> Result<OwnedSemaphorePermit> {
        let permits = byte_permits(bytes)?;
        Arc::clone(&self.event_bytes)
            .acquire_many_owned(permits)
            .await
            .map_err(stopped)
    }

    fn try_reserve_bytes(&self, bytes: usize) -> Result<OwnedSemaphorePermit> {
        let permits = byte_permits(bytes)?;
        Arc::clone(&self.event_bytes)
            .try_acquire_many_owned(permits)
            .map_err(|error| match error {
                tokio::sync::TryAcquireError::NoPermits => queue_full(),
                tokio::sync::TryAcquireError::Closed => stopped(error),
            })
    }
}

fn timestamp(event: Event) -> Result<(TimestampedEvent, usize)> {
    let event_bytes = queued_bytes(&event)?;
    Ok((
        TimestampedEvent {
            recorded_at_ms: unix_timestamp_ms()?,
            event,
        },
        event_bytes,
    ))
}

/// What one queued event charges against the byte budget. A streamed delta, the hot path,
/// charges its text plus the record around it instead of being serialized just to measure it.
fn queued_bytes(event: &Event) -> Result<usize> {
    match &event.msg {
        EventMsg::AssistantContentDelta(delta) => Ok(delta
            .delta
            .len()
            .saturating_add(std::mem::size_of::<TimestampedEvent>())),
        _ => crate::serialized_len(event),
    }
}

fn byte_permits(bytes: usize) -> Result<u32> {
    if bytes > RECORDER_EVENT_BYTE_BUDGET {
        return Err(queue_full());
    }
    u32::try_from(bytes).map_err(|error| Error::Stopped(format!("{RECORDER_QUEUE_FULL}: {error}")))
}

fn queue_full() -> Error {
    Error::Stopped(RECORDER_QUEUE_FULL.into())
}

fn stopped(error: impl std::fmt::Display) -> Error {
    Error::Stopped(format!("event recorder stopped: {error}"))
}

fn reply(result: oneshot::Sender<Result<()>>, outcome: Result<()>) {
    if let Err(Err(error)) = result.send(outcome) {
        tracing::warn!(%error, "event recorder caller stopped before receiving its failure");
    }
}

#[expect(
    clippy::significant_drop_tightening,
    reason = "the stream batch and current command are loop state; each branch drops them at its own transition"
)]
async fn run_recorder(
    checkpoints: Arc<dyn CheckpointStore>,
    session_id: SessionId,
    mut commands: mpsc::Receiver<RecorderCommand>,
    events: mpsc::Sender<JournalEvent>,
    stats: watch::Sender<Option<CommittedRunStats>>,
) {
    let mut terminal_error = None;
    let mut pending: Option<StreamBatch> = None;
    let mut last_stream = None;
    loop {
        let command = tokio::select! {
            biased;
            () = tokio::time::sleep_until(pending.as_ref().map_or_else(Instant::now, |batch| batch.deadline)), if pending.is_some() => {
                terminal_error = flush_stream(&mut pending, &checkpoints, &session_id, &events).await.err();
                continue;
            }
            command = commands.recv() => command,
        };
        let Some(command) = command else {
            flush_on_shutdown(&mut pending, &checkpoints, &session_id, &events).await;
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
                    reply(result, Ok(()));
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
                    reply(result, Ok(()));
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
                if !record_save(*command, &checkpoints, &events, &stats).await {
                    return;
                }
                None
            }
            RecorderCommand::Flush(result) => {
                reply(result, Ok(()));
                None
            }
        };
    }
}

async fn flush_on_shutdown(
    pending: &mut Option<StreamBatch>,
    checkpoints: &Arc<dyn CheckpointStore>,
    session_id: &SessionId,
    events: &mpsc::Sender<JournalEvent>,
) {
    if let Err(failure) = flush_stream(pending, checkpoints, session_id, events).await {
        tracing::warn!(error = %failure.message, "event recorder failed to flush streamed output on shutdown");
    }
}

/// Saves one checkpoint with its events; returns whether the recorder may continue.
async fn record_save(
    command: SaveCommand,
    checkpoints: &Arc<dyn CheckpointStore>,
    events: &mpsc::Sender<JournalEvent>,
    stats_sender: &watch::Sender<Option<CommittedRunStats>>,
) -> bool {
    let SaveCommand {
        checkpoint,
        stats,
        transcript_delta,
        execution,
        events: pending,
        byte_permit: _byte_permit,
        result,
    } = command;
    let recorded = match checkpoints
        .save_with_events(checkpoint, transcript_delta, execution, pending)
        .await
    {
        Ok(recorded) => recorded,
        Err(error) => {
            reply(result, Err(error));
            return false;
        }
    };
    publish_committed_save(recorded, stats, events, stats_sender, result).await
}

async fn publish_committed_save(
    recorded: Vec<JournalEvent>,
    stats: Option<Arc<RunStats>>,
    events: &mpsc::Sender<JournalEvent>,
    stats_sender: &watch::Sender<Option<CommittedRunStats>>,
    result: oneshot::Sender<Result<()>>,
) -> bool {
    if let Some(stats) = stats {
        let Some(last) = recorded.last() else {
            // Storage already committed; a delivery failure cannot roll that write back.
            reply(result, Ok(()));
            tracing::warn!(
                "committed run statistics have no journal receipt; stopping the recorder"
            );
            return false;
        };
        stats_sender.send_replace(Some(CommittedRunStats {
            after_sequence: last.sequence,
            stats,
        }));
    }
    let published = publish_all(recorded, events).await;
    reply(result, Ok(()));
    match published {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "saved checkpoint events could not reach the frontend; stopping the recorder");
            false
        }
    }
}

async fn flush_stream(
    pending: &mut Option<StreamBatch>,
    checkpoints: &Arc<dyn CheckpointStore>,
    session_id: &SessionId,
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
    session_id: &SessionId,
    events: &mpsc::Sender<JournalEvent>,
) -> std::result::Result<(), RecorderFailure> {
    let AppendCommand {
        event,
        byte_permit: _byte_permit,
        result,
    } = command;
    let recorded = match checkpoints
        .append_event(session_id, event.recorded_at_ms, event.event)
        .await
    {
        Ok(recorded) => recorded,
        Err(error) => {
            let terminal = RecorderFailure::from(&error);
            if let Some(result) = result {
                reply(result, Err(error));
            }
            return Err(terminal);
        }
    };
    let published = publish(recorded, events).await;
    if let Some(result) = result {
        reply(result, Ok(()));
    }
    published.map_err(|error| RecorderFailure::from(&error))
}

struct RecorderFailure {
    message: String,
}

impl From<&Error> for RecorderFailure {
    fn from(error: &Error) -> Self {
        let message = match error {
            Error::Stopped(message) => message.clone(), // rules-allow: the failure outlives the borrowed error
            error => error.to_string(),
        };
        Self { message }
    }
}

impl RecorderFailure {
    fn error(&self) -> Error {
        Error::Stopped(self.message.clone()) // rules-allow: one failure rejects every later command
    }
}

fn reject(command: RecorderCommand, failure: &RecorderFailure) -> bool {
    match command {
        RecorderCommand::Append(command) => {
            if let Some(result) = command.result {
                reply(result, Err(failure.error()));
            }
            false
        }
        RecorderCommand::Save(command) => {
            reply(command.result, Err(failure.error()));
            false
        }
        RecorderCommand::Flush(result) => {
            reply(result, Err(failure.error()));
            true
        }
    }
}

async fn publish(record: JournalEvent, events: &mpsc::Sender<JournalEvent>) -> Result<()> {
    events
        .send(record)
        .await
        .map_err(|error| Error::Stopped(format!("frontend event channel closed: {error}")))
}

async fn publish_all(
    records: Vec<JournalEvent>,
    events: &mpsc::Sender<JournalEvent>,
) -> Result<()> {
    for record in records {
        events
            .send(record)
            .await
            .map_err(|error| Error::Stopped(format!("frontend event channel closed: {error}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ToolCall, WarningEvent};
    use futures_util::FutureExt as _;

    fn warning(message: String) -> Event {
        Event {
            submission_id: Some("submission".into()),
            msg: EventMsg::Warning(WarningEvent {
                message: message.into(),
            }),
        }
    }

    fn ingress(commands: mpsc::Sender<RecorderCommand>) -> RecorderIngress {
        RecorderIngress {
            commands,
            event_bytes: Arc::new(Semaphore::new(RECORDER_EVENT_BYTE_BUDGET)),
            stats: watch::channel(None).1,
        }
    }

    #[tokio::test]
    async fn committed_save_stays_successful_when_delivery_cannot_continue() {
        for missing_receipt in [false, true] {
            let (events, receiver) = mpsc::channel(1);
            drop(receiver);
            let (stats, latest) = watch::channel(None);
            let (result, committed) = oneshot::channel();
            let recorded = if missing_receipt {
                Vec::new()
            } else {
                vec![JournalEvent {
                    sequence: 1,
                    recorded_at_ms: 0,
                    event: warning("committed".into()),
                    stream_metrics: Vec::new(),
                }]
            };
            assert!(
                !publish_committed_save(
                    recorded,
                    Some(Arc::new(RunStats::default())),
                    &events,
                    &stats,
                    result,
                )
                .await,
                "failed delivery stops the recorder"
            );
            assert!(
                committed.await.unwrap().is_ok(),
                "durable success must not trigger a rollback"
            );
            assert_eq!(latest.borrow().is_some(), !missing_receipt);
        }
    }

    #[tokio::test]
    async fn invalid_checkpoint_is_rejected_before_it_reaches_the_store() {
        let (commands, mut receiver) = mpsc::channel(1);
        let mut checkpoint = Checkpoint::empty("session".parse().expect("session ID"));
        Arc::make_mut(&mut checkpoint.session_context).owner_id = "test-bot".into();
        checkpoint.pending_tools.push(ToolCall {
            call_id: "call".into(),
            name: "tool".into(),
            arguments: serde_json::json!({}),
        });
        let error = ingress(commands)
            .save(Arc::new(checkpoint), &[], None, Vec::new())
            .now_or_never()
            .expect("rejected without waiting for the recorder")
            .expect_err("pending tool without an execution");
        assert!(error.to_string().contains("no active execution"), "{error}");
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn byte_budget_admits_its_exact_limit_and_releases_on_drop() {
        let (commands, mut receiver) = mpsc::channel(2);
        let ingress = ingress(commands);
        let overhead = crate::serialized_len(&warning(String::new())).expect("event overhead");
        ingress
            .try_record(warning("x".repeat(RECORDER_EVENT_BYTE_BUDGET - overhead)))
            .expect("exact budget fits");
        assert_eq!(
            ingress
                .try_record(warning(String::new()))
                .expect_err("byte budget exhausted")
                .to_string(),
            queue_full().to_string()
        );
        drop(receiver.try_recv().expect("queued event"));
        ingress
            .try_record(warning(String::new()))
            .expect("released byte budget");
        assert_eq!(
            ingress
                .try_record(warning(
                    "x".repeat(RECORDER_EVENT_BYTE_BUDGET - overhead + 1)
                ))
                .expect_err("over budget")
                .to_string(),
            queue_full().to_string()
        );
    }
}
