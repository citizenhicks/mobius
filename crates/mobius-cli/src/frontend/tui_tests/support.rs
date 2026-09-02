use super::*;

pub(super) fn rendered(block: FrontendBlock) -> RenderedBlock {
    RenderedBlock {
        capability: "test".into(),
        block,
    }
}

pub(super) fn recorded(
    event: EventMsg,
    blocks: Vec<RenderedBlock>,
    preview: Option<RenderedPreview>,
) -> RecordedEvent {
    RecordedEvent {
        sequence: 1,
        recorded_at_ms: 0,
        event: Event {
            submission_id: None,
            msg: event,
        },
        stream_metrics: Vec::new(),
        blocks,
        preview,
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
        Vec::new(),
        Some(RenderedPreview {
            id: id.into(),
            title: "agent".into(),
            subtitle: "complete · kimi/high · Full context".into(),
            page_id: page_id.into(),
            update,
            events: messages
                .iter()
                .map(|message| RenderedEvent {
                    recorded_at_ms: 0,
                    event: EventMsg::Message(MessageEvent {
                        author: MessageAuthor::User,
                        delivery: MessageDelivery::Turn,
                        text: (*message).into(),
                        attachments: Vec::new(),
                        message_target: None,
                    }),
                    blocks: Vec::new(),
                })
                .collect(),
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
    Op::CapabilityCommand {
        capability: "subagents".into(),
        command: "subagents".into(),
        arguments: arguments.into(),
        input: None,
        target: None,
    }
}

pub(super) fn catalog(workspace: &std::path::Path) -> UiCatalog {
    UiCatalog::build(&[], workspace).expect("UI catalog")
}

pub(super) fn default_catalog() -> UiCatalog {
    catalog(std::path::Path::new("/missing-mobius-test-workspace"))
}

pub(super) fn state() -> TuiState {
    TuiState::new(
        &default_catalog(),
        "/work/mobius".into(),
        ModelInfo {
            model: "kimi-k3".into(),
            reasoning_effort: Some("high".into()),
        },
        "kimi".into(),
        "MÖBIUS\nmodel: kimi-k3 · high".into(),
    )
}

pub(super) fn rendered_text(lines: &[ratatui::text::Line<'_>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
