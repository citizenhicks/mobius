use super::transcript::TranscriptReducer;
use super::{
    Arc, CheckpointStore, Error, Event, EventMsg, EventPageRequest, FrontendEvent,
    FrontendExtensions, JournalDelivery, JournalEvent, JournalSequence, LoadedReplay,
    MAX_REPLAY_BYTES, REPLAY_CAPACITY, REPLAY_LOAD_PAGE_SIZE, RecordedEvent, Rejection,
    RenderedBlock, RenderedPreview, Result, ServerFrame, ServerMessage, SessionHistoryPage,
    SessionWidgets, SharedFrame, VecDeque, broadcast, event_turn_page, internal,
};
use crate::wire::{HistoryCursor, TranscriptOp};
use mobius::SessionId;

/// Journal events read per page while looking for a fork point.
const FORK_SCAN_PAGE_SIZE: usize = 256;

#[derive(Clone)]
pub(super) struct ReplayEntry {
    pub(super) frame: SharedFrame,
    bytes: usize,
}

impl ReplayEntry {
    pub(super) fn new(frame: ServerFrame) -> Result<Self> {
        #[cfg(test)]
        FRAME_SIZE_MEASUREMENTS.with(|count| count.set(count.get() + 1));
        let frame = SharedFrame::encoded(frame)?;
        // Charge both the logical event (estimated by its JSON size) and cached bytes.
        let bytes = frame.encoded_len().saturating_mul(2);
        Ok(Self { frame, bytes })
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static FRAME_SIZE_MEASUREMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) async fn load_replay(
    checkpoints: &dyn CheckpointStore,
    session_id: &SessionId,
    frontend: &impl RenderEvents,
) -> Result<LoadedReplay> {
    let mut latest_sequence = 0;
    let mut before_sequence = None;
    let mut newest_first = Vec::with_capacity(REPLAY_CAPACITY);
    let mut loaded_bytes = 0_usize;
    let mut has_earlier = false;
    'pages: loop {
        let remaining = REPLAY_CAPACITY.saturating_sub(newest_first.len());
        if remaining == 0 {
            has_earlier = true;
            break;
        }
        let page = checkpoints
            .event_page(
                session_id,
                EventPageRequest {
                    before_sequence,
                    limit: remaining.min(REPLAY_LOAD_PAGE_SIZE),
                },
            )
            .await?;
        if before_sequence.is_none() {
            latest_sequence = page.latest_sequence;
        }
        let next_before_sequence = page.next_before_sequence;
        for journal in page.events {
            latest_sequence = latest_sequence.max(journal.sequence);
            // Frames also carry the event's transcript ops; the window evicts them below.
            loaded_bytes = loaded_bytes.saturating_add(serde_json::to_vec(&journal)?.len());
            if loaded_bytes > MAX_REPLAY_BYTES {
                has_earlier = true;
                break 'pages;
            }
            newest_first.push(journal);
        }
        let Some(cursor) = next_before_sequence else {
            break;
        };
        before_sequence = Some(cursor);
    }
    let mut events = newest_first;
    events.reverse();
    let mut transcript = TranscriptReducer::new(0);
    if has_earlier {
        // The window starts at a turn, so every reduced turn is whole.
        let start = events
            .iter()
            .position(|journal| matches!(journal.event.msg, EventMsg::TurnStarted(_)))
            .unwrap_or(0);
        let left_out: Vec<_> = events.drain(..start).collect();
        transcript.trim_before(events.first().map_or_else(
            || latest_sequence.saturating_add(1),
            |journal| journal.sequence,
        ));
        // Agent starts after the last turn left out still mark the first kept turn as
        // resumed; only the reducer sees them, so no stale configuration is replayed.
        transcript.follow_turn();
        let gap = left_out.iter().rev().take_while(|journal| {
            !matches!(
                journal.event.msg,
                EventMsg::TurnStarted(_) | EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
            )
        });
        for journal in
            gap.filter(|journal| matches!(journal.event.msg, EventMsg::SessionConfigured(_)))
        {
            transcript.apply(
                journal.sequence,
                journal.recorded_at_ms,
                &journal.event,
                Vec::new(),
            );
        }
    } else if let Some(parent) = checkpoints
        .session_summary(session_id)
        .await?
        .and_then(|summary| summary.parent_session_id)
    {
        transcript.fork_point(parent);
    }
    let mut replay = VecDeque::with_capacity(events.len());
    let mut replay_bytes = 0_usize;
    let mut truncated = false;
    for journal in events {
        let (record, blocks) = project_record(frontend, journal);
        let ops = transcript.apply(
            record.sequence,
            record.recorded_at_ms,
            &record.event,
            blocks,
        );
        let frame = ServerFrame::new(ServerMessage::AgentEvent {
            session_id: session_id.share(),
            record,
            transcript: ops,
        });
        if replayable(&frame) {
            truncated |= push_replay(&mut replay, &mut replay_bytes, &ReplayEntry::new(frame)?);
        }
    }
    if truncated {
        transcript.trim_before(
            replay
                .front()
                .and_then(|entry| event_sequence(&entry.frame))
                .unwrap_or_else(|| latest_sequence.saturating_add(1)),
        );
    }
    let mut widgets = SessionWidgets::new();
    let mut token_usage = None;
    for entry in &replay {
        let ServerMessage::AgentEvent { record, .. } = &entry.frame.message else {
            continue;
        };
        update_widgets(&mut widgets, &record.event.msg);
        update_token_usage(&mut token_usage, &record.event.msg);
    }
    Ok(LoadedReplay {
        latest_sequence,
        replay,
        replay_bytes,
        transcript,
        widgets,
        token_usage,
    })
}

/// Renders journal events into the blocks a frontend shows.
pub(super) trait RenderEvents: Sync {
    fn render(&self, event: &EventMsg) -> Vec<RenderedBlock>;

    /// Fills each approval call's preview; framework-only presentation has none.
    fn preview_approvals(&self, _request: &mut mobius::protocol::ExecApprovalRequestEvent) {}
}

impl RenderEvents for FrontendExtensions {
    fn render(&self, event: &EventMsg) -> Vec<RenderedBlock> {
        Self::render(self, event)
    }

    fn preview_approvals(&self, request: &mut mobius::protocol::ExecApprovalRequestEvent) {
        Self::preview_approvals(self, request);
    }
}

/// Framework-owned blocks only, for chats read without loading their middleware.
pub(super) struct FrameworkPresentation;

impl RenderEvents for FrameworkPresentation {
    fn render(&self, event: &EventMsg) -> Vec<RenderedBlock> {
        event.presentation().into_iter().collect()
    }
}

/// One complete durable turn page, reduced for clients. A cursor naming a chat this one
/// was forked from reads that chat's history above the fork point; the chat's own first
/// turn page carries the fork point and continues into it.
pub(super) async fn durable_history_page(
    checkpoints: &dyn CheckpointStore,
    session_id: &SessionId,
    frontend: &impl RenderEvents,
    before: Option<HistoryCursor>,
) -> std::result::Result<SessionHistoryPage, Rejection> {
    let (chat, depth, before_sequence) = match before {
        None => (session_id.share(), 0, None),
        Some(cursor) => {
            let depth = fork_depth(checkpoints, session_id, &cursor.session_id).await?;
            (cursor.session_id, depth, Some(cursor.before_sequence))
        }
    };
    let page = event_turn_page(checkpoints, &chat, before_sequence)
        .await
        .map_err(internal)?;
    let next_before_sequence = page.next_before_sequence;
    let mut transcript = TranscriptReducer::new(depth);
    if next_before_sequence.is_some() {
        transcript.follow_turn();
    }
    for journal in page.into_chronological() {
        let blocks = frontend.render(&journal.event.msg);
        transcript.apply(
            journal.sequence,
            journal.recorded_at_ms,
            &journal.event,
            blocks,
        );
    }
    if depth > 0 {
        transcript.finish_open_turns();
    }
    let next = match next_before_sequence {
        Some(before_sequence) => Some(HistoryCursor {
            session_id: chat,
            before_sequence,
        }),
        None => match fork_origin(checkpoints, &chat).await? {
            Some((parent, parent_sequence)) => {
                transcript.fork_point(parent.share());
                inherited_cursor(checkpoints, parent, parent_sequence).await?
            }
            None => None,
        },
    };
    Ok(SessionHistoryPage {
        page: transcript.page(),
        next,
    })
}

/// The chat `chat` was forked from and the parent checkpoint it forked at.
pub(super) async fn fork_origin(
    checkpoints: &dyn CheckpointStore,
    chat: &SessionId,
) -> std::result::Result<Option<(SessionId, u64)>, Rejection> {
    Ok(checkpoints
        .session_summary(chat)
        .await
        .map_err(internal)?
        .and_then(|summary| Some((summary.parent_session_id?, summary.parent_sequence?))))
}

/// The first inherited history page of a fork of `parent` at checkpoint
/// `parent_sequence`: the parent's turn holding its last message at or before that
/// checkpoint, or, when it has none, the same in the chat it was forked from in turn.
pub(super) async fn inherited_cursor(
    checkpoints: &dyn CheckpointStore,
    mut parent: SessionId,
    mut parent_sequence: u64,
) -> std::result::Result<Option<HistoryCursor>, Rejection> {
    loop {
        if let Some(cut) = fork_cut(checkpoints, &parent, parent_sequence).await? {
            return Ok(Some(HistoryCursor {
                session_id: parent,
                before_sequence: cut.saturating_add(1),
            }));
        }
        let Some(origin) = fork_origin(checkpoints, &parent).await? else {
            return Ok(None);
        };
        (parent, parent_sequence) = origin;
    }
}

/// The journal sequence of `chat`'s last message at or before checkpoint `sequence`.
async fn fork_cut(
    checkpoints: &dyn CheckpointStore,
    chat: &SessionId,
    sequence: u64,
) -> std::result::Result<Option<u64>, Rejection> {
    let mut before_sequence = None;
    loop {
        let page = checkpoints
            .event_page(
                chat,
                EventPageRequest {
                    before_sequence,
                    limit: FORK_SCAN_PAGE_SIZE,
                },
            )
            .await
            .map_err(internal)?;
        let cut = page.events.iter().find(|journal| {
            let target = match &journal.event.msg {
                EventMsg::Message(message) => message.message_target,
                EventMsg::AssistantMessage(message) => message.message_target,
                _ => None,
            };
            target.is_some_and(|target| target.checkpoint_sequence <= sequence)
        });
        if let Some(journal) = cut {
            return Ok(Some(journal.sequence));
        }
        let Some(next) = page.next_before_sequence else {
            return Ok(None);
        };
        before_sequence = Some(next);
    }
}

/// How many forks up from `session_id` the chat `chat` is.
async fn fork_depth(
    checkpoints: &dyn CheckpointStore,
    session_id: &SessionId,
    chat: &SessionId,
) -> std::result::Result<u32, Rejection> {
    let mut depth = 0_u32;
    let mut current = session_id.share();
    while current != *chat {
        let Some((parent, _)) = fork_origin(checkpoints, &current).await? else {
            return Err(Rejection::new(
                "invalid_history_cursor",
                "the history cursor names a chat this one was not forked from",
            ));
        };
        current = parent;
        depth = depth.saturating_add(1);
    }
    Ok(depth)
}

/// Renders the preview and reduces its nested events out of the journal event.
fn take_preview(frontend: &impl RenderEvents, event: &mut EventMsg) -> Option<RenderedPreview> {
    let EventMsg::Frontend(FrontendEvent::Preview {
        kind,
        state,
        symbol,
        duration_ms,
        started_at_ms,
        id,
        title,
        subtitle,
        page_id,
        update,
        events,
        next,
    }) = event
    else {
        return None;
    };
    let mut transcript = TranscriptReducer::new(0);
    for event in std::mem::take(events) {
        reduce_preview_event(
            frontend,
            &mut transcript,
            event.sequence,
            event.recorded_at_ms,
            event.submission_id.as_ref(),
            event.event,
        );
    }
    Some(RenderedPreview {
        kind: *kind,
        state: *state,
        symbol: symbol.clone(), // rules-allow: the journal event keeps its preview fields
        duration_ms: *duration_ms,
        started_at_ms: *started_at_ms,
        id: Arc::clone(id),
        title: Arc::clone(title),
        subtitle: Arc::clone(subtitle),
        page_id: Arc::clone(page_id),
        update: *update,
        transcript: transcript.page(),
        next: next.clone(), // rules-allow: the journal event keeps its preview fields
    })
}

/// Renders the record's blocks once, before its preview events move out, and returns
/// them for the transcript reducer beside the record.
pub(super) fn project_record(
    frontend: &impl RenderEvents,
    mut journal: JournalEvent,
) -> (RecordedEvent, Vec<RenderedBlock>) {
    let blocks = frontend.render(&journal.event.msg);
    let preview = take_preview(frontend, &mut journal.event.msg);
    if let EventMsg::ExecApprovalRequest(request) = &mut journal.event.msg {
        frontend.preview_approvals(request);
    }
    (
        RecordedEvent {
            sequence: journal.sequence,
            recorded_at_ms: journal.recorded_at_ms,
            event: journal.event,
            stream_metrics: journal.stream_metrics,
            preview,
        },
        blocks,
    )
}

pub(super) fn classify_journal_sequence(
    current: u64,
    incoming: u64,
    delivery: JournalDelivery,
) -> Result<JournalSequence> {
    if incoming <= current && delivery == JournalDelivery::LoadedStartup {
        return Ok(JournalSequence::AlreadyLoaded);
    }
    let expected = current
        .checked_add(1)
        .ok_or_else(|| Error::Config("event sequence overflow".into()))?;
    if incoming != expected {
        return Err(Error::Mobius(mobius::Error::Checkpoint(format!(
            "event journal delivery sequence is {incoming}, expected {expected}"
        ))));
    }
    Ok(JournalSequence::Next)
}

pub(super) fn validate_gateway_event(event: &EventMsg) -> Result<()> {
    if matches!(event, EventMsg::SessionHistory(_)) {
        return Err(Error::Protocol(
            "gateway agents must emit canonical events instead of nested session history".into(),
        ));
    }
    Ok(())
}

/// Reduces one preview event, with a session history's nested events in its place.
fn reduce_preview_event(
    frontend: &impl RenderEvents,
    transcript: &mut TranscriptReducer,
    sequence: u64,
    recorded_at_ms: i64,
    submission_id: Option<&Arc<str>>,
    event: EventMsg,
) {
    match event {
        EventMsg::SessionHistory(history) => {
            for nested in history.events {
                reduce_preview_event(
                    frontend,
                    transcript,
                    sequence,
                    recorded_at_ms,
                    submission_id,
                    nested,
                );
            }
        }
        EventMsg::Frontend(
            FrontendEvent::Widget { .. }
            | FrontendEvent::RemoveWidget { .. }
            | FrontendEvent::Picker { .. }
            | FrontendEvent::Preview { .. }
            | FrontendEvent::OpenSession { .. },
        ) => {}
        msg => {
            let blocks = frontend.render(&msg);
            let event = Event {
                submission_id: submission_id.map(Arc::clone),
                msg,
            };
            transcript.apply(sequence, recorded_at_ms, &event, blocks);
        }
    }
}

pub(super) fn event_sequence(frame: &ServerFrame) -> Option<u64> {
    match frame.message {
        ServerMessage::AgentEvent { ref record, .. } => Some(record.sequence),
        _ => None,
    }
}

pub(super) fn attention_count(widgets: &SessionWidgets) -> u32 {
    let count = widgets
        .iter()
        .flat_map(|(_, widget)| widget.pending_attention_items())
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Keeps the latest token count, which a fresh open reads from the session's ready payload.
pub(super) fn update_token_usage(
    token_usage: &mut Option<Arc<mobius::protocol::TokenUsageInfo>>,
    event: &EventMsg,
) {
    if let EventMsg::TokenCount(count) = event
        && let Some(info) = &count.info
    {
        *token_usage = Some(Arc::new(info.clone())); // rules-allow: the journal frame keeps its event; the session keeps the latest usage
    }
}

pub(super) fn update_widgets(widgets: &mut SessionWidgets, event: &EventMsg) {
    match event {
        EventMsg::Frontend(FrontendEvent::Widget { capability, item }) => {
            if let Some((_, current)) = widgets
                .iter_mut()
                .find(|(owner, widget)| owner == capability && widget.id == item.id)
            {
                *current = Arc::clone(item);
            } else {
                widgets.push((Arc::clone(capability), Arc::clone(item)));
            }
        }
        EventMsg::Frontend(FrontendEvent::RemoveWidget { capability, id }) => {
            widgets.retain(|(owner, widget)| owner != capability || *widget.id != **id);
        }
        _ => {}
    }
}

pub(super) fn record_and_publish(
    replay: &mut VecDeque<ReplayEntry>,
    replay_bytes: &mut usize,
    events: &broadcast::Sender<SharedFrame>,
    entry: ReplayEntry,
    suppress_broadcast: bool,
) -> bool {
    let truncated = replayable(&entry.frame) && push_replay(replay, replay_bytes, &entry);
    if !suppress_broadcast {
        broadcast_frame(events, entry.frame);
    }
    truncated
}

/// Keeps a replayable frame within the window, evicting the oldest; reports whether the
/// window lost anything.
fn push_replay(
    replay: &mut VecDeque<ReplayEntry>,
    replay_bytes: &mut usize,
    entry: &ReplayEntry,
) -> bool {
    let mut truncated = false;
    while replay.len() >= REPLAY_CAPACITY
        || replay_bytes.saturating_add(entry.bytes) > MAX_REPLAY_BYTES
    {
        let Some(discarded) = replay.pop_front() else {
            break;
        };
        *replay_bytes = replay_bytes.saturating_sub(discarded.bytes);
        truncated = true;
    }
    if entry.bytes <= MAX_REPLAY_BYTES {
        *replay_bytes = replay_bytes.saturating_add(entry.bytes);
        replay.push_back(ReplayEntry {
            frame: entry.frame.clone(), // rules-allow: the replay buffer and the broadcast share the frame handle
            bytes: entry.bytes,
        });
    } else {
        // Still deliver the event live; reconnects can reload it from the journal.
        truncated = true;
    }
    truncated
}

/// Drops a completed step's delta frames that only appended text: the step's final
/// `Item` op carries the whole text, while frames that created items or rows stay.
pub(super) fn compact_replay_deltas(
    replay: &mut VecDeque<ReplayEntry>,
    replay_bytes: &mut usize,
    model_step_id: &str,
) {
    replay.retain(|entry| {
        let remove = matches!(
            &entry.frame.message,
            ServerMessage::AgentEvent {
                record: RecordedEvent {
                    event: Event {
                        msg: EventMsg::AssistantContentDelta(delta),
                        ..
                    },
                    ..
                },
                transcript,
                ..
            } if delta.model_step_id.as_ref() == model_step_id
                && transcript
                    .iter()
                    .all(|op| matches!(op, TranscriptOp::AppendText { .. }))
        );
        if remove {
            *replay_bytes = replay_bytes.saturating_sub(entry.bytes);
        }
        !remove
    });
}

pub(super) fn replayable(frame: &ServerFrame) -> bool {
    !matches!(
        &frame.message,
        ServerMessage::AgentEvent {
            record: RecordedEvent {
                event: Event {
                    msg: EventMsg::SessionResumeRequested(_)
                        | EventMsg::Frontend(
                            FrontendEvent::Preview { .. }
                                | FrontendEvent::Picker { .. }
                                | FrontendEvent::Widget { .. }
                                | FrontendEvent::RemoveWidget { .. }
                                | FrontendEvent::OpenSession { .. }
                        ),
                    ..
                },
                ..
            },
            ..
        }
    )
}

pub(super) fn publish_ready_and_pending(
    events: &broadcast::Sender<SharedFrame>,
    ready: ServerFrame,
    pending: Vec<SharedFrame>,
) {
    broadcast_frame(events, SharedFrame::new(ready));
    for frame in pending {
        broadcast_frame(events, frame);
    }
}

fn broadcast_frame(events: &broadcast::Sender<SharedFrame>, frame: SharedFrame) {
    if let Err(error) = events.send(frame) {
        tracing::debug!(%error, "session event had no subscribers");
    }
}
