use super::{
    Arc, CheckpointStore, Error, Event, EventMsg, EventPageRequest, FrontendEvent,
    FrontendExtensions, FrontendPreviewEvent, JournalDelivery, JournalEvent, JournalSequence,
    LoadedReplay, MAX_REPLAY_BYTES, REPLAY_CAPACITY, REPLAY_LOAD_PAGE_SIZE, RecordedEvent,
    Rejection, RenderedBlock, RenderedEvent, RenderedPreview, Result, ServerFrame, ServerMessage,
    SessionHistoryPage, SessionWidgets, SharedFrame, VecDeque, broadcast, event_turn_page,
    internal,
};

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
    session_id: &str,
    frontend: &FrontendExtensions,
) -> Result<LoadedReplay> {
    let shared_session_id: Arc<str> = session_id.into();
    let mut latest_sequence = 0;
    let mut before_sequence = None;
    let mut scanned = 0;
    let mut newest_first = VecDeque::with_capacity(REPLAY_CAPACITY);
    let mut replay_bytes = 0_usize;
    let mut has_earlier = false;
    'pages: loop {
        let remaining = REPLAY_CAPACITY.saturating_sub(scanned);
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
        if scanned == 0 {
            latest_sequence = page.latest_sequence;
        }
        let next_before_sequence = page.next_before_sequence;
        for journal in page.events {
            scanned += 1;
            latest_sequence = latest_sequence.max(journal.sequence);
            let frame = ServerFrame::new(ServerMessage::AgentEvent {
                session_id: Arc::clone(&shared_session_id),
                record: project_record(frontend, journal),
            });
            if !replayable(&frame) {
                continue;
            }
            let entry = ReplayEntry::new(frame)?;
            let frame_bytes = entry.bytes;
            if replay_bytes.saturating_add(frame_bytes) > MAX_REPLAY_BYTES {
                has_earlier = true;
                break 'pages;
            }
            replay_bytes = replay_bytes.saturating_add(frame_bytes);
            newest_first.push_back(entry);
        }
        let Some(cursor) = next_before_sequence else {
            break;
        };
        before_sequence = Some(cursor);
    }
    let replay = newest_first.into_iter().rev().collect::<VecDeque<_>>();
    let next_before_sequence = if has_earlier {
        replay
            .front()
            .and_then(|entry| event_sequence(&entry.frame))
            .or_else(|| latest_sequence.checked_add(1))
    } else {
        None
    };
    let mut widgets = SessionWidgets::new();
    for entry in &replay {
        let ServerMessage::AgentEvent { record, .. } = &entry.frame.message else {
            continue;
        };
        update_widgets(&mut widgets, &record.event.msg);
    }
    Ok(LoadedReplay {
        latest_sequence,
        replay,
        replay_bytes,
        next_before_sequence,
        widgets,
    })
}

/// Renders journal events into the blocks a frontend shows.
pub(super) trait RenderEvents: Sync {
    fn render(&self, event: &EventMsg) -> Vec<RenderedBlock>;
}

impl RenderEvents for FrontendExtensions {
    fn render(&self, event: &EventMsg) -> Vec<RenderedBlock> {
        Self::render(self, event)
    }
}

/// Framework-owned blocks only, for chats read without loading their middleware.
pub(super) struct FrameworkPresentation;

impl RenderEvents for FrameworkPresentation {
    fn render(&self, event: &EventMsg) -> Vec<RenderedBlock> {
        event.presentation().into_iter().collect()
    }
}

/// One complete durable turn page, projected for clients.
pub(super) async fn durable_history_page(
    checkpoints: &dyn CheckpointStore,
    session_id: &str,
    frontend: &impl RenderEvents,
    before_sequence: Option<u64>,
) -> std::result::Result<SessionHistoryPage, Rejection> {
    let page = event_turn_page(checkpoints, session_id, before_sequence)
        .await
        .map_err(internal)?;
    let next_before_sequence = page.next_before_sequence;
    let records = page
        .into_chronological()
        .into_iter()
        .map(|event| project_record(frontend, event))
        .collect();
    Ok(SessionHistoryPage {
        records,
        next_before_sequence,
    })
}

/// Renders the preview and moves its nested events out of the journal event.
fn take_preview(frontend: &impl RenderEvents, event: &mut EventMsg) -> Option<RenderedPreview> {
    let EventMsg::Frontend(FrontendEvent::Preview {
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
    let mut flattened = Vec::new();
    for event in std::mem::take(events) {
        flatten_preview_event(
            event.event,
            event.submission_id.as_ref(),
            event.recorded_at_ms,
            &mut flattened,
        );
    }
    Some(RenderedPreview {
        symbol: symbol.clone(), // rules-allow: the journal event keeps its preview fields
        duration_ms: *duration_ms,
        started_at_ms: *started_at_ms,
        id: Arc::clone(id),
        title: Arc::clone(title),
        subtitle: Arc::clone(subtitle),
        page_id: Arc::clone(page_id),
        update: *update,
        events: flattened
            .into_iter()
            .map(|event| RenderedEvent {
                submission_id: event.submission_id,
                blocks: frontend.render(&event.event),
                recorded_at_ms: event.recorded_at_ms,
                event: event.event,
            })
            .collect(),
        next: next.clone(), // rules-allow: the journal event keeps its preview fields
    })
}

pub(super) fn project_record(
    frontend: &impl RenderEvents,
    mut journal: JournalEvent,
) -> RecordedEvent {
    let blocks = frontend.render(&journal.event.msg);
    let preview = take_preview(frontend, &mut journal.event.msg);
    RecordedEvent {
        sequence: journal.sequence,
        recorded_at_ms: journal.recorded_at_ms,
        event: journal.event,
        stream_metrics: journal.stream_metrics,
        blocks,
        preview,
    }
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

fn flatten_preview_event(
    event: EventMsg,
    submission_id: Option<&Arc<str>>,
    recorded_at_ms: i64,
    flattened: &mut Vec<FrontendPreviewEvent>,
) {
    match event {
        EventMsg::SessionHistory(history) => {
            for nested in history.events {
                flatten_preview_event(nested, submission_id, recorded_at_ms, flattened);
            }
        }
        EventMsg::Frontend(
            FrontendEvent::Widget { .. }
            | FrontendEvent::RemoveWidget { .. }
            | FrontendEvent::Picker { .. }
            | FrontendEvent::Preview { .. },
        ) => {}
        event => flattened.push(FrontendPreviewEvent {
            submission_id: submission_id.map(Arc::clone),
            recorded_at_ms,
            event,
        }),
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
    let ReplayEntry {
        frame,
        bytes: frame_bytes,
    } = entry;
    let mut truncated = false;
    if replayable(&frame) {
        while replay.len() >= REPLAY_CAPACITY
            || replay_bytes.saturating_add(frame_bytes) > MAX_REPLAY_BYTES
        {
            let Some(discarded) = replay.pop_front() else {
                break;
            };
            *replay_bytes = replay_bytes.saturating_sub(discarded.bytes);
            truncated = true;
        }
        if frame_bytes <= MAX_REPLAY_BYTES {
            *replay_bytes = replay_bytes.saturating_add(frame_bytes);
            replay.push_back(ReplayEntry {
                frame: frame.clone(), // rules-allow: the replay buffer and the broadcast share the frame handle
                bytes: frame_bytes,
            });
        } else {
            // Still deliver the event live; reconnects can reload it from the journal.
            truncated = true;
        }
    }
    if !suppress_broadcast {
        broadcast_frame(events, frame);
    }
    truncated
}

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
                ..
            } if delta.model_step_id.as_ref() == model_step_id
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
