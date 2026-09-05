use super::*;

pub(super) async fn load_replay(
    checkpoints: &dyn CheckpointStore,
    session_id: &str,
    frontend: &FrontendExtensions,
) -> Result<LoadedReplay> {
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
                session_id: session_id.into(),
                record: project_record(frontend, journal),
            });
            if !replayable(&frame) {
                continue;
            }
            let frame_bytes = validate_event_frame(&frame)?;
            if replay_bytes.saturating_add(frame_bytes) > MAX_REPLAY_BYTES {
                has_earlier = true;
                break 'pages;
            }
            replay_bytes = replay_bytes.saturating_add(frame_bytes);
            newest_first.push_back(frame);
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
            .and_then(event_sequence)
            .or_else(|| latest_sequence.checked_add(1))
    } else {
        None
    };
    let mut widgets = SessionWidgets::new();
    for frame in &replay {
        let ServerMessage::AgentEvent { record, .. } = &frame.message else {
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

pub(super) fn render_preview(
    frontend: &FrontendExtensions,
    event: &EventMsg,
) -> Option<RenderedPreview> {
    let EventMsg::Frontend(FrontendEvent::Preview {
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
    Some(RenderedPreview {
        id: id.clone(),
        title: title.clone(),
        subtitle: subtitle.clone(),
        page_id: page_id.clone(),
        update: *update,
        events: flatten_preview(events)
            .into_iter()
            .map(|event| RenderedEvent {
                submission_id: event.submission_id,
                blocks: frontend.render(&event.event),
                recorded_at_ms: event.recorded_at_ms,
                event: event.event,
            })
            .collect(),
        next: next.clone(),
    })
}

pub(super) fn project_record(
    frontend: &FrontendExtensions,
    mut journal: JournalEvent,
) -> RecordedEvent {
    let (blocks, preview) = project_event(frontend, &journal.event.msg);
    if preview.is_some() {
        clear_projected_preview_events(&mut journal.event.msg);
    }
    RecordedEvent {
        sequence: journal.sequence,
        recorded_at_ms: journal.recorded_at_ms,
        event: journal.event,
        stream_metrics: journal.stream_metrics,
        blocks,
        preview,
    }
}

pub(super) fn clear_projected_preview_events(event: &mut EventMsg) {
    if let EventMsg::Frontend(FrontendEvent::Preview { events, .. }) = event {
        events.clear();
    }
}

pub(super) fn project_event(
    frontend: &FrontendExtensions,
    event: &EventMsg,
) -> (Vec<RenderedBlock>, Option<RenderedPreview>) {
    (frontend.render(event), render_preview(frontend, event))
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

pub(super) fn flatten_preview(events: &[FrontendPreviewEvent]) -> Vec<FrontendPreviewEvent> {
    let mut flattened = Vec::new();
    for event in events.iter().cloned() {
        flatten_preview_event(event, &mut flattened);
    }
    flattened
}

pub(super) fn flatten_preview_event(
    event: FrontendPreviewEvent,
    flattened: &mut Vec<FrontendPreviewEvent>,
) {
    let recorded_at_ms = event.recorded_at_ms;
    let submission_id = event.submission_id;
    match event.event {
        EventMsg::SessionHistory(history) => {
            for nested in history.events {
                flatten_preview_event(
                    FrontendPreviewEvent {
                        submission_id: submission_id.clone(),
                        recorded_at_ms,
                        event: nested,
                    },
                    flattened,
                );
            }
        }
        EventMsg::Frontend(
            FrontendEvent::Widget { .. }
            | FrontendEvent::RemoveWidget { .. }
            | FrontendEvent::Picker { .. }
            | FrontendEvent::Preview { .. },
        ) => {}
        event => flattened.push(FrontendPreviewEvent {
            submission_id,
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

pub(super) fn update_widgets(widgets: &mut SessionWidgets, event: &EventMsg) {
    match event {
        EventMsg::Frontend(FrontendEvent::Widget { capability, item }) => {
            let key = (capability.clone(), item.id.clone());
            if let Some((_, current)) = widgets.iter_mut().find(|(candidate, _)| candidate == &key)
            {
                *current = item.clone();
            } else {
                widgets.push((key, item.clone()));
            }
        }
        EventMsg::Frontend(FrontendEvent::RemoveWidget { capability, id }) => {
            widgets.retain(|((owner, widget), _)| owner != capability || widget != id);
        }
        _ => {}
    }
}

pub(super) fn record_and_publish(
    replay: &mut VecDeque<ServerFrame>,
    replay_bytes: &mut usize,
    events: &broadcast::Sender<ServerFrame>,
    frame: ServerFrame,
    suppress_broadcast: bool,
) -> Result<bool> {
    let frame_bytes = validate_event_frame(&frame)?;
    let mut truncated = false;
    if replayable(&frame) {
        while replay.len() >= REPLAY_CAPACITY
            || replay_bytes.saturating_add(frame_bytes) > MAX_REPLAY_BYTES
        {
            let Some(discarded) = replay.pop_front() else {
                break;
            };
            *replay_bytes = replay_bytes.saturating_sub(serde_json::to_vec(&discarded)?.len());
            truncated = true;
        }
        *replay_bytes = replay_bytes.saturating_add(frame_bytes);
        replay.push_back(frame.clone());
    }
    if !suppress_broadcast {
        let _ = events.send(frame);
    }
    Ok(truncated)
}

pub(super) fn compact_replay_deltas(
    replay: &mut VecDeque<ServerFrame>,
    replay_bytes: &mut usize,
    model_step_id: &str,
) -> Result<()> {
    replay.retain(|frame| {
        !matches!(
            &frame.message,
            ServerMessage::AgentEvent {
                record: RecordedEvent {
                    event: Event {
                        msg: EventMsg::AssistantContentDelta(delta),
                        ..
                    },
                    ..
                },
                ..
            } if delta.model_step_id == model_step_id
        )
    });
    *replay_bytes = replay.iter().try_fold(0_usize, |total, frame| {
        Ok::<_, Error>(total.saturating_add(serde_json::to_vec(frame)?.len()))
    })?;
    Ok(())
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

pub(super) fn validate_event_frame(frame: &ServerFrame) -> Result<usize> {
    let frame_bytes = serde_json::to_vec(frame)?.len();
    if frame_bytes > MAX_FRAME_BYTES {
        return Err(Error::Protocol(format!(
            "agent event exceeds the {MAX_FRAME_BYTES}-byte gateway frame limit"
        )));
    }
    Ok(frame_bytes)
}

pub(super) fn publish_ready_and_pending(
    events: &broadcast::Sender<ServerFrame>,
    ready: ServerFrame,
    pending: Vec<ServerFrame>,
) {
    let _ = events.send(ready);
    for frame in pending {
        let _ = events.send(frame);
    }
}
