use super::*;

pub(super) fn rendered(block: FrontendBlock) -> RenderedBlock {
    RenderedBlock {
        capability: "test".into(),
        block,
    }
}

pub(super) fn recorded(event: EventMsg, preview: Option<RenderedPreview>) -> RecordedEvent {
    RecordedEvent {
        sequence: 1,
        recorded_at_ms: 0,
        event: Event {
            submission_id: preview.as_ref().map(|_| "preview-request".into()),
            msg: event,
        },
        stream_metrics: Vec::new(),
        preview,
    }
}

/// Applies an agent event that carries no transcript ops of its own.
pub(super) fn gateway_event(state: &mut TuiState, record: RecordedEvent, live: bool) {
    events::handle_gateway_event(state, record, Vec::new(), live);
}

/// A page of user messages, as the gateway reduces them.
pub(super) fn message_page(messages: &[&str]) -> TranscriptPage {
    TranscriptPage {
        items: messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                Arc::new(TranscriptItem {
                    id: format!("message:{index}").into(),
                    turn_id: None,
                    recorded_at_ms: 0,
                    state: TranscriptItemState::Complete,
                    terminal: false,
                    starts_turn: false,
                    resumed: false,
                    inherited: false,
                    target: None,
                    content: TranscriptItemContent::Message {
                        message: MessageEvent {
                            author: MessageAuthor::User,
                            delivery: MessageDelivery::Turn,
                            text: (*message).into(),
                            attachments: Vec::new(),
                            reply: None,
                            message_target: None,
                        },
                    },
                })
            })
            .collect(),
        rows: Vec::new(),
    }
}

pub(super) fn preview_record(
    id: &str,
    page_id: &str,
    update: FrontendPreviewUpdate,
    messages: &[&str],
    next: Option<Op>,
) -> RecordedEvent {
    recorded(
        EventMsg::ContextCompacted,
        Some(RenderedPreview {
            kind: mobius::protocol::FrontendPreviewKind::Agent,
            state: None,
            symbol: None,
            duration_ms: None,
            started_at_ms: None,
            id: id.into(),
            title: "agent".into(),
            subtitle: "complete · kimi/high · Full context".into(),
            page_id: page_id.into(),
            update,
            transcript: message_page(messages),
            next,
        }),
    )
}

pub(super) fn snapshot(state: &TuiState) -> &SnapshotPreview {
    let Some(PreviewState {
        content: PreviewContent::Snapshot(snapshot),
        ..
    }) = state.preview.as_ref()
    else {
        panic!("snapshot preview");
    };
    snapshot
}

pub(super) fn preview_messages(snapshot: &SnapshotPreview) -> Vec<&str> {
    snapshot
        .transcript
        .iter()
        .map(|entry| entry.text.as_str())
        .collect()
}

pub(super) fn preview_continuation(arguments: &str) -> Op {
    Op::command("subagents", "subagents", arguments)
}

pub(super) fn catalog(workspace: &std::path::Path) -> UiCatalog {
    UiCatalog::build(&[], Some(workspace)).expect("UI catalog")
}

pub(super) fn default_catalog() -> UiCatalog {
    catalog(std::path::Path::new("/missing-mobius-test-workspace"))
}

pub(super) fn state() -> TuiState {
    TuiState::new(
        &default_catalog(),
        Some("/work/mobius".into()),
        ModelInfo {
            model: "kimi-k3".into(),
            reasoning_effort: Some("high".into()),
        },
        "kimi".into(),
        "MÖBIUS\nmodel: kimi-k3 · high".into(),
    )
}
